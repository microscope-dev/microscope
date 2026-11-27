//! Zero-copy, 64-byte-aligned binary table container (`USCOPEIM`) for cached
//! per-binary debug indexes.

pub mod backing;
pub mod container;
pub mod format;
pub mod strings;
mod validate;

pub use backing::AlignedBytes;
pub use strings::{
    PathId, Paths, PathsBuilder, StrId, Strings, StringsBuilder, valid_pool, valid_reference,
};
pub use validate::{ImageError, Limits};

/// Sentinel index for absent optional row or string references.
///
/// Reserving `u32::MAX` keeps `MAX_ROWS` strictly below `2^32` so every valid
/// index is distinct from `NONE`.
pub const NONE: u32 = u32::MAX;

pub const MAX_ROWS: u64 = NONE as u64;

#[derive(Debug, Clone, Copy)]
pub struct Placed {
    pub offset: usize,
    pub length: usize,
}
