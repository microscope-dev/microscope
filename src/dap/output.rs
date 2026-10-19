//! Forwards a pipe's bytes to the client as `output` events.

use std::os::fd::OwnedFd;

use serde_json::json;
use tokio::io::AsyncReadExt as _;
use tokio::net::unix::pipe;
use tokio::task::JoinHandle;

use super::session::Client;

/// The most bytes one `output` event carries.
const CHUNK: usize = 32 * 1024;

/// Creates a pipe whose write end becomes a child's stream and whose read
/// end is forwarded once [`spawn`] is called.
pub fn pipe() -> std::io::Result<(OwnedFd, OwnedFd)> {
    Ok(nix::unistd::pipe2(nix::fcntl::OFlag::O_CLOEXEC)?)
}

/// Forwards everything written to the pipe until every writer closes it.
/// Text is decoded as UTF-8, replacing invalid bytes, without splitting a
/// character between events.
pub fn spawn(
    read: OwnedFd,
    category: &'static str,
    client: Client,
) -> std::io::Result<JoinHandle<()>> {
    let mut receiver = pipe::Receiver::from_owned_fd(read)?;
    Ok(tokio::spawn(async move {
        let mut pending = Vec::with_capacity(CHUNK);
        let mut buffer = vec![0; CHUNK];
        loop {
            let read = receiver.read(&mut buffer).await.unwrap_or(0);
            pending.extend_from_slice(&buffer[..read]);
            let text = decode(&mut pending, read == 0);

            // Replacement characters can make the text longer than the bytes.
            for piece in pieces(&text, CHUNK) {
                if client
                    .event("output", json!({"category": category, "output": piece}))
                    .await
                    .is_err()
                {
                    return;
                }
            }

            if read == 0 {
                return;
            }
        }
    }))
}

/// Splits text into pieces of at most `limit` bytes without splitting a
/// character.
pub fn pieces(text: &str, limit: usize) -> impl Iterator<Item = &str> {
    let mut rest = text;
    std::iter::from_fn(move || {
        if rest.is_empty() {
            return None;
        }

        let mut end = rest.len().min(limit);
        while !rest.is_char_boundary(end) {
            end -= 1;
        }

        let (piece, after) = rest.split_at(end);
        rest = after;
        Some(piece)
    })
}

/// Takes the decodable prefix of `bytes`, leaving an incomplete final
/// character for the next read unless the stream has ended.
pub fn decode(bytes: &mut Vec<u8>, end: bool) -> String {
    let mut text = String::new();
    let mut rest = &bytes[..];

    loop {
        match std::str::from_utf8(rest) {
            Ok(valid) => {
                text.push_str(valid);
                rest = &[];
                break;
            }
            Err(error) => {
                let (valid, after) = rest.split_at(error.valid_up_to());
                text.push_str(&String::from_utf8_lossy(valid));
                match error.error_len() {
                    Some(length) => {
                        text.push(char::REPLACEMENT_CHARACTER);
                        rest = &after[length..];
                    }
                    None if end => {
                        text.push(char::REPLACEMENT_CHARACTER);
                        rest = &[];
                        break;
                    }
                    None => {
                        rest = after;
                        break;
                    }
                }
            }
        }
    }

    let consumed = bytes.len() - rest.len();
    bytes.drain(..consumed);
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn characters_split_between_reads_are_kept_whole() {
        let encoded = "aé✓😀".as_bytes();
        for split in 0..=encoded.len() {
            let mut pending = encoded[..split].to_vec();
            let mut text = decode(&mut pending, false);
            pending.extend_from_slice(&encoded[split..]);
            text.push_str(&decode(&mut pending, true));
            assert_eq!(text, "aé✓😀", "split at {split}");
            assert!(pending.is_empty());
        }
    }

    #[test]
    fn pieces_respect_their_limit_and_characters() {
        assert_eq!(pieces("a✓b", 3).collect::<Vec<_>>(), ["a", "✓", "b"]);
        assert_eq!(pieces("", 3).count(), 0);
    }

    #[test]
    fn invalid_bytes_are_replaced_and_a_truncated_character_ends_the_stream() {
        let mut pending = b"a\xffb\xe2\x9c".to_vec();
        assert_eq!(decode(&mut pending, false), "a\u{fffd}b");
        assert_eq!(pending, b"\xe2\x9c");
        assert_eq!(decode(&mut pending, true), "\u{fffd}");
        assert!(pending.is_empty());
    }
}
