//! How captures of nested memory are laid out, so that both engines write
//! them and every decoder reads them the same way.
//!
//! - An iovec array: for each element read, its base and length as
//!   little-endian `u64`s, the bytes captured from it as a `u32` count,
//!   then those bytes.
//! - A string array, as `argv`: the number of elements as a `u32` (or
//!   `u32::MAX` when the array's end was not found), then each element
//!   read, NUL-terminated.

/// One element of an iovec capture.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Iovec<'a> {
    pub base: u64,
    pub len: u64,
    pub bytes: &'a [u8],
}

pub fn push_iovec(blob: &mut Vec<u8>, base: u64, len: u64, bytes: &[u8]) {
    blob.extend_from_slice(&base.to_le_bytes());
    blob.extend_from_slice(&len.to_le_bytes());
    let count = u32::try_from(bytes.len()).unwrap_or(u32::MAX);
    blob.extend_from_slice(&count.to_le_bytes());
    blob.extend_from_slice(&bytes[..count as usize]);
}

/// The elements of an iovec capture, stopping at the first malformed one.
#[must_use]
pub fn iovecs(mut blob: &[u8]) -> Vec<Iovec<'_>> {
    let mut elements = Vec::new();

    while blob.len() >= 20 {
        let base = u64::from_le_bytes(blob[..8].try_into().unwrap_or_default());
        let len = u64::from_le_bytes(blob[8..16].try_into().unwrap_or_default());
        let count = u32::from_le_bytes(blob[16..20].try_into().unwrap_or_default()) as usize;
        let Some(bytes) = blob.get(20..20 + count) else {
            break;
        };

        elements.push(Iovec { base, len, bytes });
        blob = &blob[20 + count..];
    }

    elements
}

pub fn start_strings(blob: &mut Vec<u8>, count: Option<u32>) {
    blob.extend_from_slice(&count.unwrap_or(u32::MAX).to_le_bytes());
}

pub fn push_string(blob: &mut Vec<u8>, string: &[u8]) {
    let end = string
        .iter()
        .position(|&byte| byte == 0)
        .unwrap_or(string.len());
    blob.extend_from_slice(&string[..end]);
    blob.push(0);
}

/// A string array's element count, when its end was found, and the
/// elements read.
#[must_use]
pub fn strings(blob: &[u8]) -> (Option<u32>, Vec<&[u8]>) {
    let Some(header) = blob.get(..4) else {
        return (None, Vec::new());
    };

    let count = u32::from_le_bytes(header.try_into().unwrap_or_default());
    let rest = &blob[4..];
    if rest.is_empty() {
        return ((count != u32::MAX).then_some(count), Vec::new());
    }

    let mut elements: Vec<&[u8]> = rest.split(|&byte| byte == 0).collect();

    // The terminator of the last element leaves an empty tail.
    if rest.last() == Some(&0) {
        elements.pop();
    }

    ((count != u32::MAX).then_some(count), elements)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iovecs_and_strings_round_trip() {
        let mut blob = Vec::new();
        push_iovec(&mut blob, 0x1000, 5, b"hello");
        push_iovec(&mut blob, 0x2000, 100, b"wor");
        let read = iovecs(&blob);
        assert_eq!(read.len(), 2);
        assert_eq!(
            (read[1].base, read[1].len, read[1].bytes),
            (0x2000, 100, &b"wor"[..])
        );

        let mut blob = Vec::new();
        start_strings(&mut blob, Some(3));
        push_string(&mut blob, b"ls");
        push_string(&mut blob, b"-l\0junk");
        assert_eq!(strings(&blob), (Some(3), vec![&b"ls"[..], &b"-l"[..]]));
        assert_eq!(strings(&[]), (None, Vec::new()));

        // An empty array, as an empty environment, holds no elements.
        let mut blob = Vec::new();
        start_strings(&mut blob, Some(0));
        assert_eq!(strings(&blob), (Some(0), Vec::new()));
    }
}
