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

#[derive(Debug, Default, Clone)]
pub(super) struct Pooled {
    first: foldhash::HashMap<u64, u32>,
    collided: Vec<u32>,
}

impl Pooled {
    pub(super) fn find(&self, hash: u64, same: impl Fn(u32) -> bool) -> Option<u32> {
        let first = *self.first.get(&hash)?;
        if same(first) {
            return Some(first);
        }

        self.collided.iter().copied().find(|row| same(*row))
    }

    pub(super) fn insert(&mut self, hash: u64, row: u32) {
        match self.first.entry(hash) {
            std::collections::hash_map::Entry::Occupied(_) => self.collided.push(row),
            std::collections::hash_map::Entry::Vacant(vacant) => {
                vacant.insert(row);
            }
        }
    }
}

impl StringsBuilder {
    pub fn push(&mut self, text: &str) -> Option<StrId> {
        self.push_hashed(
            text,
            std::hash::BuildHasher::hash_one(&foldhash::fast::FixedState::default(), text),
        )
    }

    pub(super) fn push_hashed(&mut self, text: &str, hash: u64) -> Option<StrId> {
        if let Some(id) = self.pooled.find(hash, |id| {
            Strings(&self.bytes).bytes(StrId(id)) == text.as_bytes()
        }) {
            return Some(StrId(id));
        }

        if text.as_bytes().contains(&0) {
            return None;
        }

        let id = StrId(u32::try_from(self.bytes.len()).ok()?);
        self.bytes.extend_from_slice(text.as_bytes());
        self.bytes.push(0);
        u32::try_from(self.bytes.len()).ok()?;
        self.pooled.insert(hash, id.0);
        Some(id)
    }

    pub fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Strings<'a>(pub &'a [u8]);

impl<'a> Strings<'a> {
    pub fn get(self, id: StrId) -> &'a str {
        let text = std::str::from_utf8(self.bytes(id));
        debug_assert!(text.is_ok(), "validation checked string {} is UTF-8", id.0);
        text.unwrap_or("\u{fffd}")
    }

    pub fn bytes(self, id: StrId) -> &'a [u8] {
        terminated(self.0, id.0)
    }

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

    pub fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Paths<'a>(pub &'a [u8]);

impl<'a> Paths<'a> {
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

pub fn valid_pool(pool: &[u8], utf8: bool) -> bool {
    if pool.last().is_some_and(|last| *last != 0) {
        return false;
    }
    !utf8 || std::str::from_utf8(pool).is_ok()
}

pub fn valid_reference(pool: &[u8], offset: u32, utf8: bool) -> bool {
    let Some(&byte) = pool.get(offset as usize) else {
        return false;
    };

    !utf8 || byte & 0xc0 != 0x80
}
