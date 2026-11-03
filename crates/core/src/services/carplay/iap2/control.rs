//! The control session's messages: `40 40`, the message's length and id, then
//! its parameters, each its length, id and data. Lengths are big-endian and
//! count their own header (6 bytes for a message, 4 for a parameter).
//! Parameters can hold parameters (groups); strings end with a NUL byte.

const START: [u8; 2] = [0x40, 0x40];
const MESSAGE_HEADER: usize = 6;
const PARAMETER_HEADER: usize = 4;

pub const REQUEST_AUTHENTICATION_CERTIFICATE: u16 = 0xAA00;
pub const AUTHENTICATION_CERTIFICATE: u16 = 0xAA01;
pub const REQUEST_AUTHENTICATION_CHALLENGE_RESPONSE: u16 = 0xAA02;
pub const AUTHENTICATION_RESPONSE: u16 = 0xAA03;
pub const AUTHENTICATION_FAILED: u16 = 0xAA04;
pub const AUTHENTICATION_SUCCEEDED: u16 = 0xAA05;

pub const START_IDENTIFICATION: u16 = 0x1D00;
pub const IDENTIFICATION_INFORMATION: u16 = 0x1D01;
pub const IDENTIFICATION_ACCEPTED: u16 = 0x1D02;
pub const IDENTIFICATION_REJECTED: u16 = 0x1D03;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Parameter {
    pub id: u16,
    pub data: Vec<u8>,
}

impl Parameter {
    pub fn bytes(id: u16, data: &[u8]) -> Self {
        Self {
            id,
            data: data.to_vec(),
        }
    }

    /// A parameter whose presence is its meaning.
    pub fn flag(id: u16) -> Self {
        Self::bytes(id, &[])
    }

    pub fn string(id: u16, text: &str) -> Self {
        let mut data = text.as_bytes().to_vec();
        data.push(0);
        Self { id, data }
    }

    pub fn u8(id: u16, value: u8) -> Self {
        Self::bytes(id, &[value])
    }

    pub fn u16(id: u16, value: u16) -> Self {
        Self::bytes(id, &value.to_be_bytes())
    }

    pub fn u32(id: u16, value: u32) -> Self {
        Self::bytes(id, &value.to_be_bytes())
    }

    pub fn group(id: u16, parameters: &[Parameter]) -> Self {
        Self {
            id,
            data: encode_parameters(parameters),
        }
    }

    /// The text of a string parameter, without its NUL.
    pub fn text(&self) -> Option<&str> {
        let text = self.data.strip_suffix(&[0]).unwrap_or(&self.data);
        std::str::from_utf8(text).ok()
    }

    /// The parameters a group holds.
    pub fn members(&self) -> Result<Vec<Parameter>, String> {
        decode_parameters(&self.data)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Message {
    pub id: u16,
    pub parameters: Vec<Parameter>,
}

impl Message {
    pub fn new(id: u16, parameters: Vec<Parameter>) -> Self {
        Self { id, parameters }
    }

    pub fn parameter(&self, id: u16) -> Option<&Parameter> {
        self.parameters.iter().find(|p| p.id == id)
    }

    pub fn encode(&self) -> Vec<u8> {
        let parameters = encode_parameters(&self.parameters);
        let mut message = Vec::with_capacity(MESSAGE_HEADER + parameters.len());
        message.extend_from_slice(&START);
        message.extend_from_slice(&((MESSAGE_HEADER + parameters.len()) as u16).to_be_bytes());
        message.extend_from_slice(&self.id.to_be_bytes());
        message.extend_from_slice(&parameters);
        message
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, String> {
        if bytes.len() < MESSAGE_HEADER || bytes[..2] != START {
            return Err("not a control session message".into());
        }
        let length = u16::from_be_bytes([bytes[2], bytes[3]]) as usize;
        if length < MESSAGE_HEADER || length > bytes.len() {
            return Err(format!(
                "control message of {length} bytes in {} received",
                bytes.len()
            ));
        }
        Ok(Self {
            id: u16::from_be_bytes([bytes[4], bytes[5]]),
            parameters: decode_parameters(&bytes[MESSAGE_HEADER..length])?,
        })
    }
}

fn encode_parameters(parameters: &[Parameter]) -> Vec<u8> {
    let mut bytes = Vec::new();
    for parameter in parameters {
        let length = (PARAMETER_HEADER + parameter.data.len()) as u16;
        bytes.extend_from_slice(&length.to_be_bytes());
        bytes.extend_from_slice(&parameter.id.to_be_bytes());
        bytes.extend_from_slice(&parameter.data);
    }
    bytes
}

fn decode_parameters(mut bytes: &[u8]) -> Result<Vec<Parameter>, String> {
    let mut parameters = Vec::new();
    while !bytes.is_empty() {
        if bytes.len() < PARAMETER_HEADER {
            return Err("truncated control message parameter".into());
        }
        let length = u16::from_be_bytes([bytes[0], bytes[1]]) as usize;
        if length < PARAMETER_HEADER || length > bytes.len() {
            return Err(format!(
                "control message parameter of {length} bytes in {} left",
                bytes.len()
            ));
        }
        parameters.push(Parameter {
            id: u16::from_be_bytes([bytes[2], bytes[3]]),
            data: bytes[PARAMETER_HEADER..length].to_vec(),
        });
        bytes = &bytes[length..];
    }
    Ok(parameters)
}
