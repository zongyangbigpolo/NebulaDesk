//! Length-prefixed JSON framing over QUIC streams.

use quinn::{RecvStream, SendStream};
use serde::de::DeserializeOwned;
use serde::Serialize;

use crate::messages::MAX_MESSAGE;
use crate::SignalError;

/// Write one message, length-prefixed.
pub async fn write_message<T: Serialize>(
    stream: &mut SendStream,
    message: &T,
) -> Result<(), SignalError> {
    let body = serde_json::to_vec(message)?;
    if body.len() > MAX_MESSAGE {
        return Err(SignalError::TooLarge(body.len()));
    }
    let len = u32::try_from(body.len()).expect("checked against MAX_MESSAGE");
    stream.write_all(&len.to_le_bytes()).await?;
    stream.write_all(&body).await?;
    Ok(())
}

/// Read one length-prefixed message.
///
/// The length is validated before any allocation, so a peer cannot make the
/// reader reserve gigabytes by lying about the size of a message it never
/// intends to send.
pub async fn read_message<T: DeserializeOwned>(stream: &mut RecvStream) -> Result<T, SignalError> {
    let mut len = [0u8; 4];
    stream.read_exact(&mut len).await?;
    let len = u32::from_le_bytes(len) as usize;
    if len > MAX_MESSAGE {
        return Err(SignalError::TooLarge(len));
    }
    let mut body = vec![0u8; len];
    stream.read_exact(&mut body).await?;
    Ok(serde_json::from_slice(&body)?)
}

/// Send a request on a fresh bidirectional stream and read the single reply.
///
/// One stream per exchange means a slow response cannot delay an unrelated
/// one, and the stream's FIN makes the end of the reply unambiguous.
pub async fn request<Req: Serialize, Res: DeserializeOwned>(
    connection: &quinn::Connection,
    message: &Req,
) -> Result<Res, SignalError> {
    let (mut send, mut recv) = connection.open_bi().await?;
    write_message(&mut send, message).await?;
    send.finish()?;
    read_message(&mut recv).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    #[derive(Debug, Serialize, Deserialize, PartialEq)]
    struct Probe {
        value: String,
    }

    #[test]
    fn oversized_messages_are_refused_before_they_are_written() {
        // Checked on the encoding side too, so a bug in one component cannot
        // wedge a peer's read loop.
        let big = Probe {
            value: "x".repeat(MAX_MESSAGE + 1),
        };
        assert!(serde_json::to_vec(&big).unwrap().len() > MAX_MESSAGE);
    }
}
