#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ImageError {
    #[error("the image is larger than an image may be")]
    TooLarge,

    #[error("the image is truncated")]
    Truncated,

    #[error("the bytes are not an image")]
    NotAnImage,

    #[error("the image's format version is {found}, not {expected}")]
    Format { found: u32, expected: u32 },

    #[error("the image was normalized by revision {found}, not {expected}")]
    Normalization { found: u32, expected: u32 },

    #[error("the image's records are laid out differently")]
    Layout,

    #[error("the image's checksum does not match its contents")]
    Checksum,

    #[error("the image is malformed: {0}")]
    Malformed(String),
}

#[derive(Debug, Clone, Copy)]
pub struct Limits {
    pub bytes: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            bytes: super::backing::MAX_INPUT,
        }
    }
}
