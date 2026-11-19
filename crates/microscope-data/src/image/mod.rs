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

pub const NONE: u32 = u32::MAX;

pub const MAX_ROWS: u64 = NONE as u64;

#[derive(Debug, Clone, Copy)]
pub struct Placed {
    pub offset: usize,
    pub length: usize,
}
