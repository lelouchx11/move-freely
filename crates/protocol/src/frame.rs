//! Length-prefixed framing for [`Message`](crate::Message) and binary transfer
//! chunks over any async stream.
//!
//! Wire format:
//! - 4-byte big-endian unsigned payload length `N` (including the 1-byte kind tag)
//! - 1-byte frame kind tag:
//!   - `0x01`: JSON-serialized [`Message`]
//!   - `0x02`: Raw binary chunk (`transfer_id: u64 BE` + `file_index: u32 BE` +
//!     `offset: u64 BE` + `is_last: u8` + `data: [u8]`)
//!
//! Binary chunks avoid base64/JSON-array expansion during image and file
//! clipboard transfers while sharing the same framing safety bounds.

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::message::Message;

/// Hard cap on a single frame's payload (8 MiB).
pub const MAX_FRAME_LEN: u32 = 8 * 1024 * 1024;

const KIND_JSON_MESSAGE: u8 = 0x01;
const KIND_BINARY_CHUNK: u8 = 0x02;
const CHUNK_HEADER_LEN: usize = 8 + 4 + 8 + 1;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BinaryChunk {
    pub transfer_id: u64,
    pub file_index: u32,
    pub offset: u64,
    pub is_last: bool,
    pub data: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum WireFrame {
    Message(Message),
    Chunk(BinaryChunk),
}

#[derive(Debug, thiserror::Error)]
pub enum FrameError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("frame length {0} exceeds maximum {MAX_FRAME_LEN}")]
    TooLarge(u32),
    #[error("empty frame payload")]
    EmptyFrame,
    #[error("unknown wire frame kind 0x{0:02x}")]
    UnknownKind(u8),
    #[error("truncated binary chunk header")]
    TruncatedChunk,
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
}

/// Write a single [`WireFrame`] (JSON message or binary chunk) and flush.
pub async fn write_wire_frame<W>(stream: &mut W, frame: &WireFrame) -> Result<(), FrameError>
where
    W: AsyncWrite + Unpin,
{
    match frame {
        WireFrame::Message(msg) => {
            let payload = serde_json::to_vec(msg)?;
            let total_len = 1usize.saturating_add(payload.len());
            if total_len > MAX_FRAME_LEN as usize {
                return Err(FrameError::TooLarge(total_len as u32));
            }
            stream.write_all(&(total_len as u32).to_be_bytes()).await?;
            stream.write_all(&[KIND_JSON_MESSAGE]).await?;
            stream.write_all(&payload).await?;
        }
        WireFrame::Chunk(chunk) => {
            let total_len = 1usize
                .saturating_add(CHUNK_HEADER_LEN)
                .saturating_add(chunk.data.len());
            if total_len > MAX_FRAME_LEN as usize {
                return Err(FrameError::TooLarge(total_len as u32));
            }
            stream.write_all(&(total_len as u32).to_be_bytes()).await?;
            stream.write_all(&[KIND_BINARY_CHUNK]).await?;
            stream.write_all(&chunk.transfer_id.to_be_bytes()).await?;
            stream.write_all(&chunk.file_index.to_be_bytes()).await?;
            stream.write_all(&chunk.offset.to_be_bytes()).await?;
            stream
                .write_all(&[if chunk.is_last { 1 } else { 0 }])
                .await?;
            stream.write_all(&chunk.data).await?;
        }
    }
    stream.flush().await?;
    Ok(())
}

/// Read one [`WireFrame`]. Returns `Ok(None)` on clean EOF at a frame boundary.
pub async fn read_wire_frame<R>(stream: &mut R) -> Result<Option<WireFrame>, FrameError>
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
    if len == 0 {
        return Err(FrameError::EmptyFrame);
    }
    if len > MAX_FRAME_LEN {
        return Err(FrameError::TooLarge(len));
    }

    let mut payload = vec![0u8; len as usize];
    stream.read_exact(&mut payload).await?;

    let kind = payload[0];
    let body = &payload[1..];
    match kind {
        KIND_JSON_MESSAGE => {
            let msg = serde_json::from_slice(body)?;
            Ok(Some(WireFrame::Message(msg)))
        }
        KIND_BINARY_CHUNK => {
            if body.len() < CHUNK_HEADER_LEN {
                return Err(FrameError::TruncatedChunk);
            }
            let transfer_id = u64::from_be_bytes(body[0..8].try_into().unwrap());
            let file_index = u32::from_be_bytes(body[8..12].try_into().unwrap());
            let offset = u64::from_be_bytes(body[12..20].try_into().unwrap());
            let is_last = body[20] != 0;
            let data = body[21..].to_vec();
            Ok(Some(WireFrame::Chunk(BinaryChunk {
                transfer_id,
                file_index,
                offset,
                is_last,
                data,
            })))
        }
        other => Err(FrameError::UnknownKind(other)),
    }
}

/// Convenience helper: write a JSON [`Message`] frame and flush.
pub async fn write_frame<W>(stream: &mut W, msg: &Message) -> Result<(), FrameError>
where
    W: AsyncWrite + Unpin,
{
    write_wire_frame(stream, &WireFrame::Message(msg.clone())).await
}

/// Convenience helper: read the next [`Message`] frame (skipping binary chunks if unexpected).
pub async fn read_frame<R>(stream: &mut R) -> Result<Option<Message>, FrameError>
where
    R: AsyncRead + Unpin,
{
    loop {
        match read_wire_frame(stream).await? {
            None => return Ok(None),
            Some(WireFrame::Message(msg)) => return Ok(Some(msg)),
            Some(WireFrame::Chunk(_)) => continue,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::{ArchKind, ChannelRole, DeviceId, OsKind, PeerCapabilities, PROTOCOL_VERSION};

    #[tokio::test]
    async fn frames_round_trip_over_a_duplex_pipe() {
        let (mut a, mut b) = tokio::io::duplex(64 * 1024);
        let sent = vec![
            WireFrame::Message(Message::Hello {
                device_id: DeviceId::from("dev-1"),
                name: "macbook".into(),
                os: OsKind::MacOs,
                arch: ArchKind::Aarch64,
                version: PROTOCOL_VERSION,
                channel: ChannelRole::Control,
                session_nonce: 42,
                monitors: vec![],
                capabilities: PeerCapabilities::default(),
            }),
            WireFrame::Message(Message::PointerMoveRel { dx: 5, dy: -5 }),
            WireFrame::Chunk(BinaryChunk {
                transfer_id: 99,
                file_index: 2,
                offset: 1024,
                is_last: true,
                data: vec![1, 2, 3, 4, 255],
            }),
            WireFrame::Message(Message::Heartbeat {
                seq: 1,
                sent_ms: 1000,
            }),
        ];
        let to_send = sent.clone();
        let writer = tokio::spawn(async move {
            for f in &to_send {
                write_wire_frame(&mut a, f).await.unwrap();
            }
            drop(a);
        });

        let mut received = Vec::new();
        while let Some(f) = read_wire_frame(&mut b).await.unwrap() {
            received.push(f);
        }
        writer.await.unwrap();
        assert_eq!(sent, received);
    }

    #[tokio::test]
    async fn clean_eof_yields_none() {
        let (a, mut b) = tokio::io::duplex(16);
        drop(a);
        assert!(read_wire_frame(&mut b).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn rejects_oversized_frame() {
        let (mut a, mut b) = tokio::io::duplex(64);
        a.write_all(&(MAX_FRAME_LEN + 1).to_be_bytes())
            .await
            .unwrap();
        a.flush().await.unwrap();
        let err = read_wire_frame(&mut b).await.unwrap_err();
        assert!(matches!(err, FrameError::TooLarge(_)));
    }
}
