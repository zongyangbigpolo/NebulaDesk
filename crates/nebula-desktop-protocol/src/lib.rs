//! Control-only NDJSON. Never carries media or human login credentials.
use std::io::{self, BufRead, Write};

use serde::{de::DeserializeOwned, Deserialize, Serialize};

pub const VERSION: u32 = 1;
pub const MAX_LINE_BYTES: usize = 64 * 1024;
pub const MAX_FILES: usize = 64;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Policy {
    pub input: bool,
    pub audio: bool,
    pub clipboard: bool,
    pub file_transfer: bool,
}

/// Deliberately not Debug: the signed bearer ticket must never enter logs.
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LaunchTicket {
    pub session_id: String,
    pub ticket: String,
    pub gateway_addr: String,
    pub gateway_pin: String,
    pub agent_key: String,
    #[serde(default)]
    pub policy: Policy,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Launch {
    pub version: u32,
    pub resource_id: String,
    pub resource_name: String,
    pub ticket: LaunchTicket,
}

impl Launch {
    pub fn validate(&self) -> io::Result<()> {
        if self.version != VERSION {
            return Err(invalid("unsupported desktop protocol version"));
        }
        if self.resource_id.is_empty()
            || self.resource_name.is_empty()
            || self.ticket.ticket.is_empty()
            || self.ticket.session_id.is_empty()
        {
            return Err(invalid("incomplete desktop launch"));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case", deny_unknown_fields)]
pub enum Command {
    Focus {},
    Disconnect {},
    SetAudio { enabled: bool },
    SetClipboard { enabled: bool },
    SendFiles { paths: Vec<String> },
}

impl Command {
    pub fn validate(&self) -> io::Result<()> {
        if let Self::SendFiles { paths } = self {
            if paths.len() > MAX_FILES || paths.iter().any(|p| p.is_empty() || p.contains('\0')) {
                return Err(invalid("invalid file selection"));
            }
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionState {
    Connecting,
    Connected,
    Disconnected,
    Failed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConnectionPath {
    Direct,
    Relay,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransferDirection {
    Send,
    Receive,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransferState {
    Offered,
    Transferring,
    Complete,
    Failed,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case", deny_unknown_fields)]
pub enum Event {
    State {
        state: SessionState,
        path: Option<ConnectionPath>,
        error: Option<String>,
    },
    Metrics {
        rtt_ms: Option<f64>,
        width: u32,
        height: u32,
    },
    Transfer {
        id: String,
        name: String,
        direction: TransferDirection,
        transferred: u64,
        total: u64,
        state: TransferState,
        error: Option<String>,
    },
}

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

/// Reads a complete bounded line without first allocating untrusted input.
/// Parse errors intentionally omit serde's text (it can quote secret values).
pub fn read_message<T: DeserializeOwned>(reader: &mut impl BufRead) -> io::Result<Option<T>> {
    let mut line = Vec::new();
    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            return if line.is_empty() {
                Ok(None)
            } else {
                Err(invalid("unterminated IPC line"))
            };
        }
        let end = available.iter().position(|&b| b == b'\n');
        let count = end.map_or(available.len(), |n| n + 1);
        if line.len() + count > MAX_LINE_BYTES {
            return Err(invalid("IPC line exceeds limit"));
        }
        line.extend_from_slice(&available[..count]);
        reader.consume(count);
        if end.is_some() {
            return serde_json::from_slice(&line)
                .map(Some)
                .map_err(|_| invalid("invalid IPC message"));
        }
    }
}

pub fn write_message<T: Serialize>(writer: &mut impl Write, message: &T) -> io::Result<()> {
    let mut line = serde_json::to_vec(message).map_err(|_| invalid("cannot encode IPC message"))?;
    line.push(b'\n');
    if line.len() > MAX_LINE_BYTES {
        return Err(invalid("IPC line exceeds limit"));
    }
    writer.write_all(&line)?;
    writer.flush()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounded_reader_rejects_unterminated_oversize_and_secret_errors() {
        assert!(read_message::<Command>(&mut &b"{\"command\":\"focus\"}"[..]).is_err());
        let data = vec![b'x'; MAX_LINE_BYTES + 1];
        assert!(read_message::<Command>(&mut data.as_slice()).is_err());
        let error = read_message::<Command>(&mut &b"{\"command\":\"SECRET\"}\n"[..]).unwrap_err();
        assert!(!error.to_string().contains("SECRET"));
        assert!(read_message::<Command>(&mut &b""[..]).unwrap().is_none());
    }

    #[test]
    fn messages_are_strict_and_round_trip() {
        let command = Command::SetAudio { enabled: false };
        let mut output = Vec::new();
        write_message(&mut output, &command).unwrap();
        assert_eq!(
            read_message::<Command>(&mut output.as_slice()).unwrap(),
            Some(command)
        );
        assert!(read_message::<Command>(
            &mut &b"{\"command\":\"focus\",\"ticket\":\"secret\"}\n"[..]
        )
        .is_err());
        assert!(Command::SendFiles {
            paths: vec!["x".into(); MAX_FILES + 1]
        }
        .validate()
        .is_err());
    }
}
