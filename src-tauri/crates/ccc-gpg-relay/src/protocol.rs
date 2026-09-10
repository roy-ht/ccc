//! uplink ↔ relay 間の多重化プロトコル（フレーム定義と読み書き）。
//!
//! gpg は `gpg` / `gpgsm` / 並列 git などから同時に agent へ接続するため、
//! 1 本の stdio パイプ上で複数の assuan セッションを多重化する必要がある。
//!
//! フレーム形式（すべて big endian）:
//!
//! ```text
//! u32 frame_len | u8 type | u32 channel_id | payload...
//!                └─────────── frame_len バイト ───────────┘
//! ```
//!
//! `frame_len` は type と channel_id を含む（= 5 + payload.len()）。
//! 読み手は「4 バイト読む → frame_len バイト読む」の 2 段で 1 フレームを確定できる。

use std::io::{self, Read, Write};

use serde::{Deserialize, Serialize};

/// プロトコル版。HELLO で交換し、不一致なら即切断する。
pub const PROTOCOL_VERSION: u32 = 1;

/// 1 フレームの上限（type + channel_id + payload）。
/// 壊れた入力やプロトコル不一致で巨大な確保をしないためのガード。
pub const MAX_FRAME_LEN: usize = 1 << 20;

/// ヘッダ長（type 1 + channel_id 4）。
const HEADER_LEN: usize = 5;

/// DATA 1 フレームあたりの payload 上限（= ローカル socket からの読み取り単位）。
pub const DATA_CHUNK: usize = 32 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum FrameType {
    /// プロトコル版・役割・socket パスの交換（channel_id = 0）
    Hello = 1,
    /// acceptor 側が accept した新チャネルの通知（acceptor → connector の一方向）
    Open = 2,
    /// assuan バイト列
    Data = 3,
    /// 送信側の half close（「これ以上は送らない」）
    Close = 4,
    /// 死活（channel_id = 0）
    Ping = 5,
    Pong = 6,
}

impl FrameType {
    fn from_u8(v: u8) -> Option<Self> {
        match v {
            1 => Some(FrameType::Hello),
            2 => Some(FrameType::Open),
            3 => Some(FrameType::Data),
            4 => Some(FrameType::Close),
            5 => Some(FrameType::Ping),
            6 => Some(FrameType::Pong),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub ty: FrameType,
    pub channel: u32,
    pub payload: Vec<u8>,
}

impl Frame {
    pub fn new(ty: FrameType, channel: u32, payload: Vec<u8>) -> Self {
        Frame {
            ty,
            channel,
            payload,
        }
    }

    /// 制御フレーム（channel_id = 0）。
    pub fn control(ty: FrameType) -> Self {
        Frame::new(ty, 0, Vec::new())
    }

    pub fn open(channel: u32) -> Self {
        Frame::new(FrameType::Open, channel, Vec::new())
    }

    pub fn data(channel: u32, payload: Vec<u8>) -> Self {
        Frame::new(FrameType::Data, channel, payload)
    }

    /// half close。`reason` は診断用で、空でもよい。
    pub fn close(channel: u32, reason: &str) -> Self {
        Frame::new(FrameType::Close, channel, reason.as_bytes().to_vec())
    }

    /// このフレームがワイヤ上で占めるバイト数（送信キューの計上に使う）。
    pub fn wire_len(&self) -> usize {
        4 + HEADER_LEN + self.payload.len()
    }
}

/// HELLO の payload（JSON）。将来の拡張に備えて未知フィールドは無視する。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Hello {
    pub version: u32,
    /// "relay"（socket を所有する側） / "uplink"（ローカル agent へ繋ぐ側）
    pub role: String,
    /// 相手に見せる socket パス（診断用。動作には影響しない）
    #[serde(default)]
    pub socket_path: String,
    /// relay 側が uplink 未接続時に accept を待たせる秒数（診断用の申告）
    #[serde(default)]
    pub grace_secs: u64,
}

impl Hello {
    pub fn to_frame(&self) -> Frame {
        // JSON 化は固定構造なので失敗しない
        let payload = serde_json::to_vec(self).unwrap_or_default();
        Frame::new(FrameType::Hello, 0, payload)
    }

    pub fn from_frame(frame: &Frame) -> io::Result<Self> {
        if frame.ty != FrameType::Hello {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("HELLO を期待しましたが {:?} が届きました", frame.ty),
            ));
        }
        serde_json::from_slice(&frame.payload).map_err(|e| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("HELLO の解析に失敗: {e}"),
            )
        })
    }
}

/// フレームを 1 つ読む。相手が正常に閉じた場合は `UnexpectedEof` を返す。
pub fn read_frame<R: Read>(r: &mut R) -> io::Result<Frame> {
    let mut len_buf = [0u8; 4];
    r.read_exact(&mut len_buf)?;
    let frame_len = u32::from_be_bytes(len_buf) as usize;
    if frame_len < HEADER_LEN {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("フレーム長が短すぎます: {frame_len}"),
        ));
    }
    if frame_len > MAX_FRAME_LEN {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("フレーム長が上限を超えています: {frame_len} > {MAX_FRAME_LEN}"),
        ));
    }
    let mut buf = vec![0u8; frame_len];
    r.read_exact(&mut buf)?;
    let ty = FrameType::from_u8(buf[0]).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("未知のフレーム種別: {}", buf[0]),
        )
    })?;
    let channel = u32::from_be_bytes([buf[1], buf[2], buf[3], buf[4]]);
    Ok(Frame {
        ty,
        channel,
        payload: buf[HEADER_LEN..].to_vec(),
    })
}

/// フレームを 1 つ書く（flush まで行う）。
pub fn write_frame<W: Write>(w: &mut W, frame: &Frame) -> io::Result<()> {
    let frame_len = HEADER_LEN + frame.payload.len();
    if frame_len > MAX_FRAME_LEN {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("送信フレームが上限を超えています: {frame_len}"),
        ));
    }
    let mut buf = Vec::with_capacity(4 + frame_len);
    buf.extend_from_slice(&(frame_len as u32).to_be_bytes());
    buf.push(frame.ty as u8);
    buf.extend_from_slice(&frame.channel.to_be_bytes());
    buf.extend_from_slice(&frame.payload);
    w.write_all(&buf)?;
    w.flush()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(frame: &Frame) -> Frame {
        let mut buf = Vec::new();
        write_frame(&mut buf, frame).unwrap();
        read_frame(&mut buf.as_slice()).unwrap()
    }

    #[test]
    fn roundtrip_preserves_all_fields() {
        let f = Frame::data(7, b"OK Pleased to meet you\n".to_vec());
        assert_eq!(roundtrip(&f), f);
    }

    #[test]
    fn roundtrip_empty_payload() {
        let f = Frame::open(1);
        assert_eq!(roundtrip(&f), f);
        let f = Frame::control(FrameType::Ping);
        assert_eq!(roundtrip(&f), f);
    }

    #[test]
    fn wire_len_matches_encoded_size() {
        let f = Frame::data(3, vec![0u8; 100]);
        let mut buf = Vec::new();
        write_frame(&mut buf, &f).unwrap();
        assert_eq!(f.wire_len(), buf.len());
    }

    #[test]
    fn multiple_frames_read_in_order() {
        let frames = vec![
            Frame::open(1),
            Frame::data(1, b"abc".to_vec()),
            Frame::close(1, "eof"),
        ];
        let mut buf = Vec::new();
        for f in &frames {
            write_frame(&mut buf, f).unwrap();
        }
        let mut cursor = buf.as_slice();
        for expected in &frames {
            assert_eq!(&read_frame(&mut cursor).unwrap(), expected);
        }
    }

    #[test]
    fn hello_roundtrip() {
        let hello = Hello {
            version: PROTOCOL_VERSION,
            role: "relay".into(),
            socket_path: "/home/user/.gnupg/S.gpg-agent".into(),
            grace_secs: 5,
        };
        let parsed = Hello::from_frame(&roundtrip(&hello.to_frame())).unwrap();
        assert_eq!(parsed, hello);
    }

    #[test]
    fn hello_rejects_other_frame_types() {
        assert!(Hello::from_frame(&Frame::open(1)).is_err());
    }

    #[test]
    fn hello_tolerates_missing_optional_fields() {
        let frame = Frame::new(
            FrameType::Hello,
            0,
            br#"{"version":1,"role":"uplink"}"#.to_vec(),
        );
        let hello = Hello::from_frame(&frame).unwrap();
        assert_eq!(hello.grace_secs, 0);
        assert!(hello.socket_path.is_empty());
    }

    #[test]
    fn rejects_unknown_frame_type() {
        let mut buf = Vec::new();
        buf.extend_from_slice(&5u32.to_be_bytes());
        buf.extend_from_slice(&[99, 0, 0, 0, 1]);
        let err = read_frame(&mut buf.as_slice()).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn rejects_short_frame_len() {
        let mut buf = Vec::new();
        buf.extend_from_slice(&3u32.to_be_bytes());
        buf.extend_from_slice(&[1, 0, 0]);
        assert!(read_frame(&mut buf.as_slice()).is_err());
    }

    #[test]
    fn rejects_oversized_frame_len() {
        let mut buf = Vec::new();
        buf.extend_from_slice(&((MAX_FRAME_LEN + 1) as u32).to_be_bytes());
        let err = read_frame(&mut buf.as_slice()).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        // 上限超は payload を読む前に弾く（巨大確保をしない）
    }

    #[test]
    fn truncated_stream_is_eof() {
        let f = Frame::data(1, b"hello".to_vec());
        let mut buf = Vec::new();
        write_frame(&mut buf, &f).unwrap();
        buf.truncate(buf.len() - 2);
        let err = read_frame(&mut buf.as_slice()).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
    }

    #[test]
    fn write_rejects_oversized_payload() {
        let f = Frame::data(1, vec![0u8; MAX_FRAME_LEN]);
        let mut buf = Vec::new();
        assert!(write_frame(&mut buf, &f).is_err());
    }
}
