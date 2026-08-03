//! apcaccess.rs
//!
//! Contains functions to extract and parse the status of the apcupsd NIS.
//!
//! The NIS protocol frames every message as a 16-bit big-endian length
//! followed by that many payload bytes. The server terminates the status
//! response with a zero-length frame.

use std::collections::BTreeMap;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::timeout;

/// Command to request status from apcupsd
const CMD_STATUS: &[u8] = b"\x00\x06status";

/// Separator for key-value pairs
const SEP: char = ':';

/// Buffer size for reading from socket
const BUFFER_SIZE: usize = 1024;

/// Hard cap on the response we will buffer from a single NIS conversation.
///
/// A real status response is on the order of 1 KiB. This only exists so a
/// broken or hostile server cannot drive us out of memory.
pub const MAX_RESPONSE_BYTES: usize = 256 * 1024;

/// All supported units that can be stripped from values
const ALL_UNITS: &[&str] = &[
    "Minutes",
    "Seconds",
    "Percent Load Capacity",
    "Percent",
    "Volts",
    "Watts",
    "Amps",
    "Hz",
    "VA",
    "C",
];

/// Error type for apcaccess operations
#[derive(Debug)]
pub enum ApcAccessError {
    Io(std::io::Error),
    Timeout(u64),
    ResponseTooLarge(usize),
}

impl From<std::io::Error> for ApcAccessError {
    fn from(err: std::io::Error) -> Self {
        ApcAccessError::Io(err)
    }
}

impl std::fmt::Display for ApcAccessError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ApcAccessError::Io(e) => write!(f, "IO error: {e}"),
            ApcAccessError::Timeout(secs) => write!(f, "timed out after {secs}s"),
            ApcAccessError::ResponseTooLarge(cap) => {
                write!(f, "response exceeded {cap} bytes")
            }
        }
    }
}

impl std::error::Error for ApcAccessError {}

/// Connect to the APCUPSd NIS and request its status.
///
/// The supplied timeout bounds the whole conversation - connect, write and
/// every read - so a host that accepts the connection and then stalls cannot
/// wedge the caller indefinitely.
///
/// # Arguments
///
/// * `host` - The hostname or IP address of the apcupsd server
/// * `port` - The port number of the apcupsd NIS (default: 3551)
/// * `timeout_secs` - Overall deadline for the exchange, in seconds
///
/// # Returns
///
/// The raw framed response bytes.
pub async fn get(host: &str, port: u16, timeout_secs: u64) -> Result<Vec<u8>, ApcAccessError> {
    let deadline = Duration::from_secs(timeout_secs);
    let addr = format!("{host}:{port}");

    let exchange = async {
        let mut stream = TcpStream::connect(&addr).await?;
        stream.write_all(CMD_STATUS).await?;

        let mut buffer = Vec::new();
        let mut chunk = [0u8; BUFFER_SIZE];

        loop {
            let n = stream.read(&mut chunk).await?;
            if n == 0 {
                // Server closed before sending the terminating frame; decode
                // whatever complete frames we did receive.
                break;
            }
            if buffer.len() + n > MAX_RESPONSE_BYTES {
                return Err(ApcAccessError::ResponseTooLarge(MAX_RESPONSE_BYTES));
            }
            buffer.extend_from_slice(&chunk[..n]);

            if frames_complete(&buffer) {
                break;
            }
        }

        Ok(buffer)
    };

    match timeout(deadline, exchange).await {
        Ok(result) => result,
        Err(_) => Err(ApcAccessError::Timeout(timeout_secs)),
    }
}

/// Returns true once `buf` contains the zero-length frame that terminates a
/// status response.
fn frames_complete(buf: &[u8]) -> bool {
    let mut i = 0usize;
    while i + 2 <= buf.len() {
        let len = u16::from_be_bytes([buf[i], buf[i + 1]]) as usize;
        if len == 0 {
            return true;
        }
        i += 2;
        if i + len > buf.len() {
            return false;
        }
        i += len;
    }
    false
}

/// Decode length-prefixed frames into cleaned status lines.
///
/// Operates on raw bytes and only converts each complete frame to text, so a
/// length prefix that is not valid UTF-8 can never split a character.
/// Incomplete trailing data is ignored rather than treated as an error.
pub fn decode_frames(buf: &[u8]) -> Vec<String> {
    let mut lines = Vec::new();
    let mut i = 0usize;

    while i + 2 <= buf.len() {
        let len = u16::from_be_bytes([buf[i], buf[i + 1]]) as usize;
        if len == 0 {
            break;
        }
        i += 2;
        if i + len > buf.len() {
            break;
        }

        let line = String::from_utf8_lossy(&buf[i..i + len]);
        let line = line.trim_end_matches(['\n', '\r']).trim_end();
        if !line.is_empty() {
            lines.push(line.to_string());
        }
        i += len;
    }

    lines
}

/// Decode a raw NIS response into cleaned key/value pairs.
///
/// # Arguments
///
/// * `raw` - The raw framed response bytes
/// * `strip_units` - Whether to strip unit suffixes from the values
pub fn parse(raw: &[u8], strip_units: bool) -> BTreeMap<String, String> {
    decode_frames(raw)
        .into_iter()
        .filter_map(|line| {
            let (key, value) = line.split_once(SEP)?;
            let key = key.trim();
            if key.is_empty() {
                return None;
            }
            let value = value.trim();
            let value = if strip_units {
                strip_unit(value)
            } else {
                value
            };
            Some((key.to_string(), value.to_string()))
        })
        .collect()
}

/// Remove a trailing unit from a value, if one is present.
///
/// Matching is longest-first so `Percent Load Capacity` wins over `Percent`.
pub fn strip_unit(value: &str) -> &str {
    for unit in ALL_UNITS {
        if let Some(stripped) = value.strip_suffix(unit) {
            // Only treat it as a unit if it was space-separated, otherwise
            // "ONBATT" style values could lose their tail.
            if let Some(trimmed) = stripped.strip_suffix(' ') {
                return trimmed.trim_end();
            }
        }
    }
    value
}

/// Fetch and parse the APCUPSd status from the given host and port.
pub async fn fetch_stats(
    host: &str,
    port: u16,
    timeout_secs: u64,
    strip_units: bool,
) -> Result<BTreeMap<String, String>, ApcAccessError> {
    let raw = get(host, port, timeout_secs).await?;
    Ok(parse(&raw, strip_units))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a protocol-accurate framed response from payload lines.
    fn frame(lines: &[&str]) -> Vec<u8> {
        let mut out = Vec::new();
        for line in lines {
            let payload = format!("{line}\n");
            out.extend_from_slice(&(payload.len() as u16).to_be_bytes());
            out.extend_from_slice(payload.as_bytes());
        }
        out.extend_from_slice(&0u16.to_be_bytes());
        out
    }

    #[test]
    fn decodes_frames() {
        let raw = frame(&["APC      : 001,036,0876", "STATUS   : ONLINE"]);
        let lines = decode_frames(&raw);
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0], "APC      : 001,036,0876");
        assert_eq!(lines[1], "STATUS   : ONLINE");
    }

    #[test]
    fn parses_key_values() {
        let raw = frame(&["APC      : 001,036,0876", "STATUS   : ONLINE"]);
        let parsed = parse(&raw, false);
        assert_eq!(parsed.get("APC"), Some(&"001,036,0876".to_string()));
        assert_eq!(parsed.get("STATUS"), Some(&"ONLINE".to_string()));
    }

    #[test]
    fn strips_units() {
        let raw = frame(&[
            "LINEV    : 120.0 Volts",
            "LOADPCT  : 15.0 Percent",
            "BCHARGE  : 100.0 Percent",
            "TIMELEFT : 45.0 Minutes",
            "ITEMP    : 27.4 C",
            "NOMPOWER : 865 Watts",
            "LOADAPNT : 15.0 Percent Load Capacity",
        ]);
        let parsed = parse(&raw, true);
        assert_eq!(parsed.get("LINEV"), Some(&"120.0".to_string()));
        assert_eq!(parsed.get("LOADPCT"), Some(&"15.0".to_string()));
        assert_eq!(parsed.get("BCHARGE"), Some(&"100.0".to_string()));
        assert_eq!(parsed.get("TIMELEFT"), Some(&"45.0".to_string()));
        assert_eq!(parsed.get("ITEMP"), Some(&"27.4".to_string()));
        assert_eq!(parsed.get("NOMPOWER"), Some(&"865".to_string()));
        assert_eq!(parsed.get("LOADAPNT"), Some(&"15.0".to_string()));
    }

    #[test]
    fn keeps_values_that_merely_end_in_unit_letters() {
        // "C" is a unit, but only when space separated.
        let raw = frame(&["LASTXFER : No transfers since turnon", "SENSE    : HIGH"]);
        let parsed = parse(&raw, true);
        assert_eq!(
            parsed.get("LASTXFER"),
            Some(&"No transfers since turnon".to_string())
        );
        assert_eq!(parsed.get("SENSE"), Some(&"HIGH".to_string()));
    }

    #[test]
    fn preserves_colons_in_values() {
        let raw = frame(&["STARTTIME: 2024-01-15 10:23:45 -0500"]);
        let parsed = parse(&raw, true);
        assert_eq!(
            parsed.get("STARTTIME"),
            Some(&"2024-01-15 10:23:45 -0500".to_string())
        );
    }

    /// A line long enough that its length prefix is not valid UTF-8. The old
    /// NUL-splitting parser sliced this mid-character and panicked.
    #[test]
    fn handles_lines_longer_than_127_bytes() {
        let long_value = "x".repeat(200);
        let raw = frame(&[&format!("LASTXFER : {long_value}")]);
        // The low length byte is >= 0x80, so it is not valid UTF-8 on its own.
        // from_utf8_lossy turns it into a 3-byte U+FFFD and the old parser's
        // `x[1..]` then sliced through the middle of that character.
        assert!(raw[1] >= 0x80, "prefix byte was {:#04x}", raw[1]);
        let parsed = parse(&raw, false);
        assert_eq!(parsed.get("LASTXFER"), Some(&long_value));
    }

    #[test]
    fn handles_multibyte_utf8_payloads() {
        let raw = frame(&["MODEL    : Smart-UPS Ω 1500", "STATUS   : ONLINE"]);
        let parsed = parse(&raw, false);
        assert_eq!(parsed.get("MODEL"), Some(&"Smart-UPS Ω 1500".to_string()));
    }

    #[test]
    fn tolerates_truncated_trailing_frame() {
        let mut raw = frame(&["APC      : 001,036,0876", "STATUS   : ONLINE"]);
        raw.truncate(raw.len() - 2); // drop the terminator
        raw.extend_from_slice(&[0x00, 0x40, b'p', b'a', b'r', b't']); // claims 64 bytes, has 4
        let parsed = parse(&raw, false);
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed.get("STATUS"), Some(&"ONLINE".to_string()));
    }

    #[test]
    fn empty_input_yields_no_stats() {
        assert!(parse(&[], true).is_empty());
        assert!(parse(&[0x00], true).is_empty());
        assert!(parse(&[0x00, 0x00], true).is_empty());
    }

    #[test]
    fn frames_complete_detects_terminator() {
        let raw = frame(&["STATUS   : ONLINE"]);
        assert!(frames_complete(&raw));
        assert!(!frames_complete(&raw[..raw.len() - 2]));
        assert!(!frames_complete(&[]));
    }

    /// Serve one canned response on an ephemeral port and return its address.
    async fn serve(response: Vec<u8>) -> std::net::SocketAddr {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut request = [0u8; CMD_STATUS.len()];
            sock.read_exact(&mut request).await.unwrap();
            assert_eq!(&request[..], CMD_STATUS);
            sock.write_all(&response).await.unwrap();
        });
        addr
    }

    #[tokio::test]
    async fn fetches_over_tcp() {
        let addr = serve(frame(&["STATUS   : ONLINE", "LINEV    : 120.0 Volts"])).await;

        let stats = fetch_stats(&addr.ip().to_string(), addr.port(), 5, true)
            .await
            .unwrap();
        assert_eq!(stats.get("STATUS"), Some(&"ONLINE".to_string()));
        assert_eq!(stats.get("LINEV"), Some(&"120.0".to_string()));
    }

    #[tokio::test]
    async fn times_out_on_silent_server() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            // Accept, then never answer.
            let _sock = listener.accept().await.unwrap();
            tokio::time::sleep(Duration::from_secs(30)).await;
        });

        let err = get(&addr.ip().to_string(), addr.port(), 1)
            .await
            .unwrap_err();
        assert!(matches!(err, ApcAccessError::Timeout(1)), "got {err:?}");
    }

    #[tokio::test]
    async fn rejects_oversized_responses() {
        let mut flood = Vec::new();
        let payload = "X".repeat(1000);
        while flood.len() < MAX_RESPONSE_BYTES + 8192 {
            let line = format!("JUNK     : {payload}\n");
            flood.extend_from_slice(&(line.len() as u16).to_be_bytes());
            flood.extend_from_slice(line.as_bytes());
        }
        let addr = serve(flood).await;

        let err = get(&addr.ip().to_string(), addr.port(), 5)
            .await
            .unwrap_err();
        assert!(
            matches!(err, ApcAccessError::ResponseTooLarge(_)),
            "got {err:?}"
        );
    }

    #[tokio::test]
    async fn reports_connection_refused() {
        // Bind then drop, so the port is almost certainly closed.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);

        let err = get(&addr.ip().to_string(), addr.port(), 5)
            .await
            .unwrap_err();
        assert!(matches!(err, ApcAccessError::Io(_)), "got {err:?}");
    }

    #[test]
    fn ignores_lines_without_separator() {
        let raw = frame(&["this line has no separator", "STATUS   : ONLINE"]);
        let parsed = parse(&raw, false);
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed.get("STATUS"), Some(&"ONLINE".to_string()));
    }
}
