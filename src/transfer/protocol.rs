use super::range::Interval;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::io;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

pub const PROTOCOL_VERSION: u8 = 3;
pub const FRAME_MAGIC: [u8; 4] = *b"LSD3";
pub const HEADER_LEN: usize = 10;
pub const MAX_CONTROL_FRAME: usize = 1024 * 1024;
pub const MAX_DATA_FRAME: usize = 256 * 1024 + 8;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum FrameKind {
    Hello = 1,
    Offer = 2,
    Plan = 3,
    Range = 4,
    Data = 5,
    Pause = 6,
    Ack = 7,
    Final = 8,
    Done = 9,
    ErrorResponse = 10,
}

impl TryFrom<u8> for FrameKind {
    type Error = ProtocolError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            1 => Ok(Self::Hello),
            2 => Ok(Self::Offer),
            3 => Ok(Self::Plan),
            4 => Ok(Self::Range),
            5 => Ok(Self::Data),
            6 => Ok(Self::Pause),
            7 => Ok(Self::Ack),
            8 => Ok(Self::Final),
            9 => Ok(Self::Done),
            10 => Ok(Self::ErrorResponse),
            _ => Err(ProtocolError::UnknownFrameKind(value)),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Hello3 {
    pub protocol_version: u8,
    pub role: ConnectionRole,
    pub capabilities: Vec<String>,
    pub receiver_max_concurrency: u16,
    pub session_token: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ConnectionRole {
    Control,
    Data,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Offer3 {
    pub transfer_id: String,
    pub transfer_key: String,
    pub filename: String,
    pub total_bytes: u64,
    pub content_hash: String,
    pub sender_device_id: String,
    pub sender_name: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Plan3 {
    pub accepted: bool,
    pub session_token: String,
    pub completed_intervals: Vec<Interval>,
    pub receiver_max_concurrency: u16,
    pub max_frame_bytes: u32,
    pub reason: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Range3 {
    pub range_id: u64,
    pub start: u64,
    pub end: u64,
    pub generation: u32,
}

impl Range3 {
    pub fn interval(&self) -> Result<Interval, ProtocolError> {
        if self.start >= self.end {
            return Err(ProtocolError::InvalidRange {
                start: self.start,
                end: self.end,
            });
        }
        Ok(Interval {
            start: self.start,
            end: self.end,
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Pause3 {
    pub range_id: u64,
    pub generation: u32,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Ack3 {
    pub range_id: u64,
    pub committed_end: u64,
    pub range_hash: String,
    pub status: AckStatus,
    pub message: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum AckStatus {
    Committed,
    Duplicate,
    Rejected,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Final3 {
    pub transfer_id: String,
    pub content_hash: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Done3 {
    pub transfer_id: String,
    pub final_filename: String,
    pub total_bytes: u64,
    pub verified: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Error3 {
    pub code: String,
    pub message: String,
    pub unsupported: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Data3 {
    pub offset: u64,
    pub payload: Vec<u8>,
}

#[derive(Debug)]
pub enum ProtocolError {
    Io(io::Error),
    InvalidMagic,
    UnsupportedVersion(u8),
    UnknownFrameKind(u8),
    UnexpectedFrame {
        expected: FrameKind,
        actual: FrameKind,
    },
    FrameTooLarge {
        kind: FrameKind,
        length: usize,
        maximum: usize,
    },
    InvalidJson(serde_json::Error),
    InvalidDataFrame,
    InvalidRange {
        start: u64,
        end: u64,
    },
    InvalidOffset {
        offset: u64,
        expected: u64,
    },
}

impl std::fmt::Display for ProtocolError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{self:?}")
    }
}

impl std::error::Error for ProtocolError {}

impl From<io::Error> for ProtocolError {
    fn from(value: io::Error) -> Self {
        Self::Io(value)
    }
}

impl From<serde_json::Error> for ProtocolError {
    fn from(value: serde_json::Error) -> Self {
        Self::InvalidJson(value)
    }
}

pub async fn write_control<W, T>(
    writer: &mut W,
    kind: FrameKind,
    value: &T,
) -> Result<(), ProtocolError>
where
    W: AsyncWrite + Unpin,
    T: Serialize,
{
    if kind == FrameKind::Data {
        return Err(ProtocolError::UnexpectedFrame {
            expected: FrameKind::Hello,
            actual: kind,
        });
    }
    let payload = serde_json::to_vec(value)?;
    write_payload(writer, kind, &payload).await
}

pub async fn read_control<R, T>(reader: &mut R, expected: FrameKind) -> Result<T, ProtocolError>
where
    R: AsyncRead + Unpin,
    T: DeserializeOwned,
{
    let (kind, payload) = read_payload(reader).await?;
    if kind != expected {
        return Err(ProtocolError::UnexpectedFrame {
            expected,
            actual: kind,
        });
    }
    Ok(serde_json::from_slice(&payload)?)
}

pub async fn write_data<W>(writer: &mut W, offset: u64, payload: &[u8]) -> Result<(), ProtocolError>
where
    W: AsyncWrite + Unpin,
{
    if payload.len() + 8 > MAX_DATA_FRAME {
        return Err(ProtocolError::FrameTooLarge {
            kind: FrameKind::Data,
            length: payload.len() + 8,
            maximum: MAX_DATA_FRAME,
        });
    }
    let mut frame = Vec::with_capacity(payload.len() + 8);
    frame.extend_from_slice(&offset.to_be_bytes());
    frame.extend_from_slice(payload);
    write_payload(writer, FrameKind::Data, &frame).await
}

pub async fn read_data<R>(reader: &mut R) -> Result<Data3, ProtocolError>
where
    R: AsyncRead + Unpin,
{
    let (kind, payload) = read_payload(reader).await?;
    if kind != FrameKind::Data {
        return Err(ProtocolError::UnexpectedFrame {
            expected: FrameKind::Data,
            actual: kind,
        });
    }
    if payload.len() < 8 {
        return Err(ProtocolError::InvalidDataFrame);
    }
    let offset = u64::from_be_bytes(payload[..8].try_into().unwrap());
    Ok(Data3 {
        offset,
        payload: payload[8..].to_vec(),
    })
}

pub async fn write_payload<W>(
    writer: &mut W,
    kind: FrameKind,
    payload: &[u8],
) -> Result<(), ProtocolError>
where
    W: AsyncWrite + Unpin,
{
    let maximum = if kind == FrameKind::Data {
        MAX_DATA_FRAME
    } else {
        MAX_CONTROL_FRAME
    };
    if payload.len() > maximum {
        return Err(ProtocolError::FrameTooLarge {
            kind,
            length: payload.len(),
            maximum,
        });
    }
    let mut header = [0u8; HEADER_LEN];
    header[..4].copy_from_slice(&FRAME_MAGIC);
    header[4] = PROTOCOL_VERSION;
    header[5] = kind as u8;
    header[6..].copy_from_slice(&(payload.len() as u32).to_be_bytes());
    writer.write_all(&header).await?;
    writer.write_all(payload).await?;
    writer.flush().await?;
    Ok(())
}

pub async fn read_payload<R>(reader: &mut R) -> Result<(FrameKind, Vec<u8>), ProtocolError>
where
    R: AsyncRead + Unpin,
{
    let mut header = [0u8; HEADER_LEN];
    reader.read_exact(&mut header).await?;
    if header[..4] != FRAME_MAGIC {
        return Err(ProtocolError::InvalidMagic);
    }
    if header[4] != PROTOCOL_VERSION {
        return Err(ProtocolError::UnsupportedVersion(header[4]));
    }
    let kind = FrameKind::try_from(header[5])?;
    let length = u32::from_be_bytes(header[6..].try_into().unwrap()) as usize;
    let maximum = if kind == FrameKind::Data {
        MAX_DATA_FRAME
    } else {
        MAX_CONTROL_FRAME
    };
    if length > maximum {
        return Err(ProtocolError::FrameTooLarge {
            kind,
            length,
            maximum,
        });
    }
    let mut payload = vec![0u8; length];
    reader.read_exact(&mut payload).await?;
    Ok((kind, payload))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncWriteExt, duplex};

    #[tokio::test]
    async fn partial_and_coalesced_frames_decode_with_read_exact() {
        let (mut sender, mut receiver) = duplex(4096);
        let hello = Hello3 {
            protocol_version: 3,
            role: ConnectionRole::Control,
            capabilities: vec!["pause".into()],
            receiver_max_concurrency: 12,
            session_token: None,
        };
        let mut encoded = Vec::new();
        write_control(&mut encoded, FrameKind::Hello, &hello)
            .await
            .unwrap();
        write_control(&mut encoded, FrameKind::Hello, &hello)
            .await
            .unwrap();
        let split = encoded.len() / 3;
        sender.write_all(&encoded[..split]).await.unwrap();
        sender.write_all(&encoded[split..]).await.unwrap();
        let first: Hello3 = read_control(&mut receiver, FrameKind::Hello).await.unwrap();
        let second: Hello3 = read_control(&mut receiver, FrameKind::Hello).await.unwrap();
        assert_eq!(first, hello);
        assert_eq!(second, hello);
    }

    #[tokio::test]
    async fn oversized_frame_is_rejected_before_allocation() {
        let (mut sender, mut receiver) = duplex(128);
        let mut header = [0u8; HEADER_LEN];
        header[..4].copy_from_slice(&FRAME_MAGIC);
        header[4] = PROTOCOL_VERSION;
        header[5] = FrameKind::Offer as u8;
        header[6..].copy_from_slice(&((MAX_CONTROL_FRAME + 1) as u32).to_be_bytes());
        sender.write_all(&header).await.unwrap();
        assert!(matches!(
            read_payload(&mut receiver).await,
            Err(ProtocolError::FrameTooLarge { .. })
        ));
    }

    #[test]
    fn special_filename_round_trips_as_json() {
        let offer = Offer3 {
            transfer_id: "id".into(),
            transfer_key: "key".into(),
            filename: "pipes|and\nnewlines-文件.txt".into(),
            total_bytes: 42,
            content_hash: "hash".into(),
            sender_device_id: "sender".into(),
            sender_name: "name".into(),
        };
        let bytes = serde_json::to_vec(&offer).unwrap();
        assert_eq!(serde_json::from_slice::<Offer3>(&bytes).unwrap(), offer);
    }
}
