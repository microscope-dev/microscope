//! The Debug Adapter Protocol's framing: a header block with a
//! `Content-Length` field, a blank line, then that many bytes of UTF-8 JSON.

use tokio::io::{AsyncBufRead, AsyncBufReadExt as _, AsyncReadExt as _, AsyncWrite, AsyncWriteExt};

/// The largest message body accepted; a larger one ends the session.
pub const MAX_FRAME: usize = 64 * 1024 * 1024;

/// The largest header block accepted.
const MAX_HEADER: usize = 8 * 1024;

/// Why a message could not be read.
#[derive(Debug, thiserror::Error)]
pub enum TransportError {
    #[error("the connection closed in the middle of a message")]
    Truncated,

    #[error("a message header has no Content-Length")]
    MissingLength,

    #[error("invalid Content-Length {0:?}")]
    InvalidLength(String),

    #[error("a {0}-byte message exceeds the {MAX_FRAME}-byte limit")]
    TooLarge(usize),

    #[error("a message header exceeds {MAX_HEADER} bytes")]
    HeaderTooLarge,

    #[error("malformed message header line {0:?}")]
    MalformedHeader(String),

    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// One message body and what its header said.
#[derive(Debug, PartialEq, Eq)]
pub struct Frame {
    pub body: Vec<u8>,

    /// Whether the header carried `Origin`, which only a web browser sends.
    pub origin: bool,
}

/// Reads one message, or `None` when the stream ends cleanly between
/// messages. Header names are matched without regard to case, and unknown
/// headers are ignored.
pub async fn read_frame<R: AsyncBufRead + Unpin>(
    reader: &mut R,
) -> Result<Option<Frame>, TransportError> {
    let mut length = None;
    let mut origin = false;
    let mut header_bytes = 0;
    let mut line = Vec::new();

    loop {
        line.clear();
        let read = reader
            .take((MAX_HEADER + 1 - header_bytes) as u64)
            .read_until(b'\n', &mut line)
            .await?;
        if read == 0 {
            return if header_bytes == 0 {
                Ok(None)
            } else {
                Err(TransportError::Truncated)
            };
        }

        header_bytes += read;
        if line.last() != Some(&b'\n') {
            return Err(if header_bytes > MAX_HEADER {
                TransportError::HeaderTooLarge
            } else {
                TransportError::Truncated
            });
        }

        let text = String::from_utf8_lossy(&line);
        let text = text.trim_end_matches(['\r', '\n']);
        if text.is_empty() {
            // Blank lines before any header are tolerated as separators.
            if header_bytes == read {
                header_bytes = 0;
                continue;
            }

            break;
        }

        let (name, value) = text
            .split_once(':')
            .ok_or_else(|| TransportError::MalformedHeader(text.to_owned()))?;
        if name.trim().eq_ignore_ascii_case("content-length") {
            let value = value.trim();
            if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
                return Err(TransportError::InvalidLength(value.to_owned()));
            }

            let parsed = value
                .parse::<usize>()
                .map_err(|_| TransportError::InvalidLength(value.to_owned()))?;
            length = Some(parsed);
        } else if name.trim().eq_ignore_ascii_case("origin") {
            origin = true;
        }
    }

    let length = length.ok_or(TransportError::MissingLength)?;
    if length > MAX_FRAME {
        return Err(TransportError::TooLarge(length));
    }

    let mut body = vec![0; length];

    match reader.read_exact(&mut body).await {
        Ok(_) => Ok(Some(Frame { body, origin })),
        Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => {
            Err(TransportError::Truncated)
        }
        Err(error) => Err(error.into()),
    }
}

/// Writes one message whose body is `body`.
pub async fn write_frame<W: AsyncWrite + Unpin>(
    writer: &mut W,
    body: &[u8],
) -> std::io::Result<()> {
    writer
        .write_all(format!("Content-Length: {}\r\n\r\n", body.len()).as_bytes())
        .await?;
    writer.write_all(body).await?;
    writer.flush().await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(body: &str) -> Vec<u8> {
        format!("Content-Length: {}\r\n\r\n{body}", body.len()).into_bytes()
    }

    async fn read_all(bytes: &[u8]) -> Vec<Result<Option<Frame>, String>> {
        let mut reader = tokio::io::BufReader::new(bytes);
        let mut results = Vec::new();

        loop {
            let result = read_frame(&mut reader).await;
            let done = !matches!(result, Ok(Some(_)));
            results.push(result.map_err(|error| error.to_string()));
            if done {
                return results;
            }
        }
    }

    /// Feeds `bytes` through a reader that returns them in two parts split
    /// at every position.
    #[tokio::test]
    async fn every_split_of_a_stream_decodes_the_same_messages() {
        let bytes = [frame("{\"a\":1}"), frame("{}"), frame("[\"é\"]")].concat();
        let expected = read_all(&bytes).await;
        assert_eq!(expected.len(), 4);
        for split in 0..bytes.len() {
            let (first, second) = bytes.split_at(split);
            let (mut writer, reader) = tokio::io::duplex(1);
            let first = first.to_vec();
            let second = second.to_vec();
            let feeder = tokio::spawn(async move {
                writer.write_all(&first).await.expect("first part");
                writer.write_all(&second).await.expect("second part");
            });
            let mut reader = tokio::io::BufReader::new(reader);

            for expected in &expected {
                let actual = read_frame(&mut reader)
                    .await
                    .map_err(|error| error.to_string());
                assert_eq!(&actual, expected, "split at {split}");
            }

            feeder.await.expect("feeder");
        }
    }

    #[tokio::test]
    async fn headers_are_case_insensitive_and_unknown_ones_are_ignored() {
        let bytes = b"content-LENGTH: 2\r\nContent-Type: application/json\r\n\r\n{}";
        assert_eq!(
            read_all(bytes).await[0],
            Ok(Some(Frame {
                body: b"{}".to_vec(),
                origin: false
            }))
        );
        let bytes = b"Origin: http://evil\r\nContent-Length: 2\r\n\r\n{}";
        assert!(matches!(&read_all(bytes).await[0], Ok(Some(frame)) if frame.origin));
    }

    #[tokio::test]
    async fn malformed_headers_and_truncated_streams_are_typed_errors() {
        for (bytes, expected) in [
            (
                &b"Content-Type: x\r\n\r\n{}"[..],
                "a message header has no Content-Length",
            ),
            (
                b"Content-Length: two\r\n\r\n{}",
                "invalid Content-Length \"two\"",
            ),
            (
                b"Content-Length: -2\r\n\r\n{}",
                "invalid Content-Length \"-2\"",
            ),
            (
                b"Content-Length: 99999999999999999999999\r\n\r\n",
                "invalid Content-Length \"99999999999999999999999\"",
            ),
            (b"Content-Length:\r\n\r\n", "invalid Content-Length \"\""),
            (
                b"garbage\r\n\r\n",
                "malformed message header line \"garbage\"",
            ),
            (
                b"Content-Length: 5\r\n\r\n{}",
                "the connection closed in the middle of a message",
            ),
            (
                b"Content-Length: 5\r\n",
                "the connection closed in the middle of a message",
            ),
            (
                b"Content-Len",
                "the connection closed in the middle of a message",
            ),
            (
                b"Content-Length: 67108865\r\n\r\n",
                "a 67108865-byte message exceeds the 67108864-byte limit",
            ),
        ] {
            assert_eq!(
                read_all(bytes).await[0],
                Err(expected.to_owned()),
                "{}",
                String::from_utf8_lossy(bytes)
            );
        }

        let long = format!("X-Padding: {}\r\n", "x".repeat(MAX_HEADER));
        assert_eq!(
            read_all(long.as_bytes()).await[0],
            Err(format!("a message header exceeds {MAX_HEADER} bytes"))
        );
        assert_eq!(read_all(b"").await[0], Ok(None));
    }

    #[tokio::test]
    async fn written_lengths_count_bytes_not_characters() {
        let mut written = Vec::new();
        write_frame(&mut written, "\"é✓\"".as_bytes())
            .await
            .expect("write");
        assert_eq!(written, "Content-Length: 7\r\n\r\n\"é✓\"".as_bytes());
        assert_eq!(
            read_all(&written).await[0],
            Ok(Some(Frame {
                body: "\"é✓\"".as_bytes().to_vec(),
                origin: false
            }))
        );
    }
}
