//! Each WebSocket binary message is one zstd frame, with its content size in
//! the frame header, holding one protobuf message. The service enforces the
//! same limits.

use prost::Message;
use zstd::bulk::{Compressor, Decompressor};
use zstd::zstd_safe::{self, CParameter};

/// The largest compressed message either side may send.
pub const MAX_COMPRESSED_BYTES: usize = 1 << 20;

/// The largest message either side may send, once decompressed. The frame's
/// declared size is checked against it before decompressing, so a small
/// frame cannot expand into gigabytes.
pub const MAX_DECOMPRESSED_BYTES: usize = 4 << 20;

/// zstd's default level.
const LEVEL: i32 = 3;

#[derive(Debug, thiserror::Error)]
pub enum WireError {
    #[error("the message is {0} bytes compressed, over the limit of {MAX_COMPRESSED_BYTES}")]
    TooLarge(usize),

    #[error("the message is {0} bytes decompressed, over the limit of {MAX_DECOMPRESSED_BYTES}")]
    DecompressedTooLarge(u64),

    #[error("the message's zstd frame does not declare its size")]
    UnknownSize,

    #[error("the message is not a zstd frame")]
    NotZstd,

    #[error("decompressing the message: {0}")]
    Decompress(std::io::Error),

    #[error("the message decompressed to {actual} bytes, not the {declared} its frame declared")]
    SizeMismatch { declared: usize, actual: usize },

    #[error("decoding the message: {0}")]
    Decode(prost::DecodeError),

    #[error("compressing the message: {0}")]
    Compress(std::io::Error),
}

/// Encodes and decodes messages, reusing its zstd contexts. One serves one
/// connection.
pub struct Codec {
    compressor: Compressor<'static>,
    decompressor: Decompressor<'static>,
}

impl std::fmt::Debug for Codec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Codec").finish_non_exhaustive()
    }
}

impl Codec {
    pub fn new() -> Result<Self, WireError> {
        let mut compressor = Compressor::new(LEVEL).map_err(WireError::Compress)?;
        compressor
            .set_parameter(CParameter::ContentSizeFlag(true))
            .map_err(WireError::Compress)?;
        let decompressor = Decompressor::new().map_err(WireError::Decompress)?;
        Ok(Self {
            compressor,
            decompressor,
        })
    }

    /// Encodes `message` as one zstd frame.
    pub fn encode(&mut self, message: &impl Message) -> Result<Vec<u8>, WireError> {
        let encoded = message.encode_to_vec();
        if encoded.len() > MAX_DECOMPRESSED_BYTES {
            return Err(WireError::DecompressedTooLarge(encoded.len() as u64));
        }

        let frame = self
            .compressor
            .compress(&encoded)
            .map_err(WireError::Compress)?;
        if frame.len() > MAX_COMPRESSED_BYTES {
            return Err(WireError::TooLarge(frame.len()));
        }

        Ok(frame)
    }

    /// Decodes one zstd frame holding one message, refusing a frame over
    /// the limits before doing any work on it.
    pub fn decode<M: Message + Default>(&mut self, frame: &[u8]) -> Result<M, WireError> {
        if frame.len() > MAX_COMPRESSED_BYTES {
            return Err(WireError::TooLarge(frame.len()));
        }

        let declared = match zstd_safe::get_frame_content_size(frame) {
            Ok(Some(size)) => size,
            Ok(None) => return Err(WireError::UnknownSize),
            Err(_) => return Err(WireError::NotZstd),
        };
        let declared = usize::try_from(declared)
            .ok()
            .filter(|&size| size <= MAX_DECOMPRESSED_BYTES)
            .ok_or(WireError::DecompressedTooLarge(declared))?;

        // With the declared size as the capacity, a frame that lies about its
        // size, or a second frame after the first, fails instead of growing
        // the buffer.
        let decoded = self
            .decompressor
            .decompress(frame, declared)
            .map_err(WireError::Decompress)?;
        if decoded.len() != declared {
            return Err(WireError::SizeMismatch {
                declared,
                actual: decoded.len(),
            });
        }

        M::decode(decoded.as_slice()).map_err(WireError::Decode)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::proto::{AgentMessage, Hello, agent_message};

    fn hello() -> AgentMessage {
        AgentMessage {
            body: Some(agent_message::Body::Hello(Hello {
                protocol: 1,
                agent_version: "1.0".to_owned(),
                hostname: "workstation".to_owned(),
                accepts_commands: true,
            })),
        }
    }

    #[test]
    fn messages_round_trip() {
        let mut codec = Codec::new().unwrap();
        let frame = codec.encode(&hello()).unwrap();
        assert_eq!(codec.decode::<AgentMessage>(&frame).unwrap(), hello());
    }

    #[test]
    fn frames_declare_their_size() {
        let mut codec = Codec::new().unwrap();
        let frame = codec.encode(&hello()).unwrap();
        let declared = zstd_safe::get_frame_content_size(&frame).unwrap();
        assert_eq!(declared, Some(hello().encoded_len() as u64));
    }

    #[test]
    fn frames_without_a_size_are_refused() {
        let mut compressor = Compressor::new(LEVEL).unwrap();
        compressor
            .set_parameter(CParameter::ContentSizeFlag(false))
            .unwrap();
        let frame = compressor.compress(&hello().encode_to_vec()).unwrap();
        let mut codec = Codec::new().unwrap();
        assert!(matches!(
            codec.decode::<AgentMessage>(&frame),
            Err(WireError::UnknownSize)
        ));
    }

    #[test]
    fn frames_that_would_expand_too_far_are_refused() {
        let frame = zstd::bulk::compress(&vec![0; MAX_DECOMPRESSED_BYTES + 1], LEVEL).unwrap();
        assert!(frame.len() < 1024);
        let mut codec = Codec::new().unwrap();
        assert!(matches!(
            codec.decode::<AgentMessage>(&frame),
            Err(WireError::DecompressedTooLarge(_))
        ));
    }

    #[test]
    fn other_bytes_are_refused() {
        let mut codec = Codec::new().unwrap();
        assert!(matches!(
            codec.decode::<AgentMessage>(b"hello"),
            Err(WireError::NotZstd)
        ));
        let too_large = vec![0; MAX_COMPRESSED_BYTES + 1];
        assert!(matches!(
            codec.decode::<AgentMessage>(&too_large),
            Err(WireError::TooLarge(_))
        ));
    }

    #[test]
    fn frames_that_lie_about_their_size_are_refused() {
        let mut codec = Codec::new().unwrap();
        let mut frame = codec.encode(&hello()).unwrap();
        // A second frame after the first exceeds the declared size.
        frame.extend_from_slice(&codec.encode(&hello()).unwrap());
        assert!(codec.decode::<AgentMessage>(&frame).is_err());
    }
}
