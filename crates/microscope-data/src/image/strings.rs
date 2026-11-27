//! NUL-terminated string and path pools indexed by `u32` byte offset (`StrId`,
//! `PathId`).

use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt as _;
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct StrId(pub u32);

#[derive(Debug, Default, Clone)]
pub struct StringsBuilder {
    bytes: Vec<u8>,
    pooled: Pooled,
}

/// Flat hash-to-offset index that stores the first entry per 64-bit hash inline
/// and spills rare collisions into `collided`, avoiding per-bucket allocations.
#[derive(Debug, Default, Clone)]
pub struct Pooled {
    first: foldhash::HashMap<u64, u32>,
    collided: Vec<u32>,
}

impl Pooled {
    pub fn find(&self, hash: u64, same: impl Fn(u32) -> bool) -> Option<u32> {
        let first = *self.first.get(&hash)?;
        if same(first) {
            return Some(first);
        }

        self.collided.iter().copied().find(|row| same(*row))
    }

    pub fn insert(&mut self, hash: u64, row: u32) {
        match self.first.entry(hash) {
            std::collections::hash_map::Entry::Occupied(_) => self.collided.push(row),
            std::collections::hash_map::Entry::Vacant(vacant) => {
                vacant.insert(row);
            }
        }
    }
}

#[must_use]
pub fn hash(text: &str) -> u64 {
    std::hash::BuildHasher::hash_one(&foldhash::fast::FixedState::default(), text)
}

impl StringsBuilder {
    pub fn push(&mut self, text: &str) -> Option<StrId> {
        self.push_hashed(text, hash(text))
    }

    pub fn push_hashed(&mut self, text: &str, hash: u64) -> Option<StrId> {
        let text = text.as_bytes();
        if text.contains(&0) {
            return None;
        }

        if let Some(id) = self.pooled.find(hash, |id| {
            let start = id as usize;
            self.bytes.get(start..start + text.len()) == Some(text)
                && self.bytes.get(start + text.len()) == Some(&0)
        }) {
            return Some(StrId(id));
        }

        let id = StrId(u32::try_from(self.bytes.len()).ok()?);
        self.bytes.extend_from_slice(text);
        self.bytes.push(0);
        u32::try_from(self.bytes.len()).ok()?;
        self.pooled.insert(hash, id.0);
        Some(id)
    }

    #[must_use]
    pub fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Strings<'a>(pub &'a [u8]);

impl<'a> Strings<'a> {
    #[must_use]
    pub fn get(self, id: StrId) -> &'a str {
        let text = std::str::from_utf8(self.bytes(id));
        debug_assert!(text.is_ok(), "validation checked string {} is UTF-8", id.0);
        text.unwrap_or("\u{fffd}")
    }

    #[must_use]
    pub fn bytes(self, id: StrId) -> &'a [u8] {
        terminated(self.0, id.0)
    }

    #[must_use]
    pub fn contains(self, id: StrId) -> bool {
        valid_reference(self.0, id.0, true)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PathId(pub u32);

#[derive(Debug, Default)]
pub struct PathsBuilder {
    bytes: Vec<u8>,
    ids: foldhash::HashMap<Box<[u8]>, PathId>,
}

impl PathsBuilder {
    pub fn intern(&mut self, path: &Path) -> Option<PathId> {
        let bytes = path.as_os_str().as_bytes();
        if bytes.contains(&0) {
            return None;
        }

        if let Some(&id) = self.ids.get(bytes) {
            return Some(id);
        }

        let id = PathId(u32::try_from(self.bytes.len()).ok()?);
        self.bytes.extend_from_slice(bytes);
        self.bytes.push(0);
        u32::try_from(self.bytes.len()).ok()?;
        self.ids.insert(bytes.into(), id);
        Some(id)
    }

    #[must_use]
    pub fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Paths<'a>(pub &'a [u8]);

impl<'a> Paths<'a> {
    #[must_use]
    pub fn get(self, id: PathId) -> &'a Path {
        Path::new(OsStr::from_bytes(terminated(self.0, id.0)))
    }
}

fn terminated(pool: &[u8], start: u32) -> &[u8] {
    let rest = pool.get(start as usize..).unwrap_or_default();
    let end = memchr::memchr(0, rest);
    debug_assert!(
        end.is_some(),
        "validation checked the bytes at {start} are terminated"
    );
    rest.get(..end.unwrap_or(0)).unwrap_or_default()
}

#[must_use]
pub fn valid_pool(pool: &[u8], utf8: bool) -> bool {
    if pool.last().is_some_and(|last| *last != 0) {
        return false;
    }
    !utf8 || std::str::from_utf8(pool).is_ok()
}

/// Checks whether `offset` starts a valid entry in a pool verified by
/// `valid_pool`.
///
/// For UTF-8 pools, rejecting continuation bytes (`0b10xxxxxx`) is sufficient
/// because any non-continuation byte inside valid NUL-terminated UTF-8 begins a
/// valid UTF-8 substring up to the next NUL.
#[must_use]
pub fn valid_reference(pool: &[u8], offset: u32, utf8: bool) -> bool {
    let Some(&byte) = pool.get(offset as usize) else {
        return false;
    };

    !utf8 || byte & 0xc0 != 0x80
}
