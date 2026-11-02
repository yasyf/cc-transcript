use std::io::{self, Read};

use sonic_rs::{JsonContainerTrait, JsonValueTrait, Value};

pub const MAX_FRAME_BYTES: usize = 1024 * 1024;

#[derive(Debug)]
pub enum ProtocolError {
    Io(io::Error),
    Invalid(String),
}

impl std::fmt::Display for ProtocolError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "{error}"),
            Self::Invalid(reason) => write!(formatter, "{reason}"),
        }
    }
}

impl From<io::Error> for ProtocolError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

fn invalid(reason: &str) -> ProtocolError {
    ProtocolError::Invalid(reason.to_owned())
}

pub fn read_frame(input: &mut impl Read) -> Result<Option<(Value, usize)>, ProtocolError> {
    let mut first = [0u8; 1];
    if input.read(&mut first)? == 0 {
        return Ok(None);
    }
    let mut rest = [0u8; 3];
    input
        .read_exact(&mut rest)
        .map_err(|_| invalid("snapshot connection closed during frame"))?;
    let length = u32::from_be_bytes([first[0], rest[0], rest[1], rest[2]]) as usize;
    if length == 0 || length > MAX_FRAME_BYTES {
        return Err(invalid("snapshot frame exceeds encoded byte bound"));
    }
    let mut payload = vec![0u8; length];
    input
        .read_exact(&mut payload)
        .map_err(|_| invalid("snapshot connection closed during frame"))?;
    let frame: Value =
        sonic_rs::from_slice(&payload).map_err(|_| invalid("malformed snapshot frame"))?;
    if !frame.is_object() {
        return Err(invalid("snapshot frame must be an object"));
    }
    Ok(Some((frame, length + 4)))
}

pub fn encode(body: &str) -> Option<Vec<u8>> {
    if body.len() > MAX_FRAME_BYTES {
        return None;
    }
    let mut frame = Vec::with_capacity(body.len() + 4);
    frame.extend_from_slice(&(body.len() as u32).to_be_bytes());
    frame.extend_from_slice(body.as_bytes());
    Some(frame)
}

pub fn keys(frame: &Value) -> Vec<&str> {
    let mut keys: Vec<&str> = frame
        .as_object()
        .expect("frame object")
        .iter()
        .map(|(key, _)| key)
        .collect();
    keys.sort_unstable();
    keys
}

pub fn integer(value: &Value) -> Option<u64> {
    if !value.is_number()
        || value.as_raw_number().is_some_and(|raw| {
            raw.as_str()
                .bytes()
                .any(|byte| matches!(byte, b'.' | b'e' | b'E'))
        })
    {
        return None;
    }
    value.as_u64()
}

pub fn protocol_id(frame: &Value) -> Option<u64> {
    if integer(frame.get("protocol")?)? != 1 {
        return None;
    }
    integer(frame.get("id")?).filter(|id| *id > 0)
}
