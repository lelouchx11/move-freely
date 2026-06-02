//! Length-prefixed framing for [`Message`](crate::Message) over any async stream.
//!
//! Wire format: a 4-byte big-endian unsigned length, followed by that many
//! bytes of JSON. The length cap guards against a peer claiming an absurd
//! allocation.

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::message::Message;

/// Hard cap on a single frame's payload (1 MiB). Real input messages are tiny;
/// this only exists to reject malicious or corrupt length prefixes.
pub const MAX_FRAME_LEN: u32 = 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum FrameError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("frame length {0} exceeds maximum {MAX_FRAME_LEN}")]
    TooLarge(u32),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
}

/// Write one message as a length-prefixed JSON frame, flushing the stream.
pub async fn write_frame<W>(stream: &mut W, msg: &Message) -> Result<(), FrameError>
where
    W: AsyncWrite + Unpin,
{
    let payload = serde_json::to_vec(msg)?;
    let len = payload.len() as u32;
    if len > MAX_FRAME_LEN {
        return Err(FrameError::TooLarge(len));
    }
    stream.write_all(&len.to_be_bytes()).await?;
    stream.write_all(&payload).await?;
    stream.flush().await?;
    Ok(())
}

/// Read exactly one length-prefixed JSON frame.
///
/// Returns `Ok(None)` on a clean EOF at a frame boundary (peer closed the
/// connection), so callers can treat it as graceful disconnect.
pub async fn read_frame<R>(stream: &mut R) -> Result<Option<Message>, FrameError>
where
    R: AsyncRead + Unpin,
{
    let mut len_buf = [0u8; 4];
    match stream.read_exact(&mut len_buf).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e.into()),
    }
    let len = u32::from_be_bytes(len_buf);
    if len > MAX_FRAME_LEN {
        return Err(FrameError::TooLarge(len));
    }
    let mut payload = vec![0u8; len as usize];
    stream.read_exact(&mut payload).await?;
    let msg = serde_json::from_slice(&payload)?;
    Ok(Some(msg))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::PROTOCOL_VERSION;

    #[tokio::test]
    async fn frames_round_trip_over_a_duplex_pipe() {
        let (mut a, mut b) = tokio::io::duplex(64 * 1024);
        let sent = vec![
            Message::Hello {
                name: "n".into(),
                width: 1,
                height: 2,
                token: "t".into(),
                version: PROTOCOL_VERSION,
            },
            Message::MouseMoveRel { dx: 5, dy: -5 },
            Message::Heartbeat,
        ];
        let to_send = sent.clone();
        let writer = tokio::spawn(async move {
            for m in &to_send {
                write_frame(&mut a, m).await.unwrap();
            }
            drop(a); // signal EOF
        });

        let mut received = Vec::new();
        while let Some(m) = read_frame(&mut b).await.unwrap() {
            received.push(m);
        }
        writer.await.unwrap();
        assert_eq!(sent, received);
    }

    #[tokio::test]
    async fn clean_eof_yields_none() {
        let (a, mut b) = tokio::io::duplex(16);
        drop(a);
        assert!(read_frame(&mut b).await.unwrap().is_none());
    }
}
