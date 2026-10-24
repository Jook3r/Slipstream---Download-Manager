//! The browsers' native messaging framing: a 32-bit length in the machine's
//! byte order, then that many bytes of UTF-8 JSON.

use std::io::{self, Read, Write};

use serde::Serialize;
use serde_json::Value;

/// Browsers refuse replies larger than 1 MB.
const MAX_REPLY: usize = 1024 * 1024;
/// Generous cap on what we accept, so a bad length cannot exhaust memory.
const MAX_REQUEST: usize = 64 * 1024 * 1024;

/// Reads one message, or `None` when the browser has closed the stream.
pub fn read_message(input: &mut impl Read) -> io::Result<Option<Value>> {
    let mut len = [0u8; 4];
    match input.read_exact(&mut len) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let len = u32::from_ne_bytes(len) as usize;
    if len > MAX_REQUEST {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "message too large",
        ));
    }
    let mut body = vec![0u8; len];
    input.read_exact(&mut body)?;
    serde_json::from_slice(&body)
        .map(Some)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

pub fn write_message(output: &mut impl Write, message: &impl Serialize) -> io::Result<()> {
    let body = serde_json::to_vec(message).map_err(io::Error::other)?;
    if body.len() > MAX_REPLY {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "reply too large",
        ));
    }
    output.write_all(&(body.len() as u32).to_ne_bytes())?;
    output.write_all(&body)?;
    output.flush()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn round_trips_messages() {
        let mut wire = Vec::new();
        write_message(&mut wire, &json!({"type": "ping"})).unwrap();
        write_message(
            &mut wire,
            &json!({"type": "download", "url": "https://x/é.zip"}),
        )
        .unwrap();
        assert_eq!(&wire[..4], &15u32.to_ne_bytes());

        let mut input = wire.as_slice();
        assert_eq!(
            read_message(&mut input).unwrap(),
            Some(json!({"type": "ping"}))
        );
        assert_eq!(
            read_message(&mut input).unwrap().unwrap()["url"],
            "https://x/é.zip"
        );
        assert_eq!(read_message(&mut input).unwrap(), None);
    }

    #[test]
    fn rejects_bad_input() {
        let huge = (MAX_REQUEST as u32 + 1).to_ne_bytes();
        assert!(read_message(&mut huge.as_slice()).is_err());
        let mut truncated = 10u32.to_ne_bytes().to_vec();
        truncated.extend_from_slice(b"{}");
        assert!(read_message(&mut truncated.as_slice()).is_err());
        let mut not_json = 3u32.to_ne_bytes().to_vec();
        not_json.extend_from_slice(b"abc");
        assert!(read_message(&mut not_json.as_slice()).is_err());
    }
}
