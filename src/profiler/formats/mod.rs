//! The files other tools write and read: pprof, folded stacks, and Chrome
//! traces. Exports go through the same call tree as every view, so they
//! show what the views show.

mod chrome;
mod folded;
mod pprof;
mod proto;
#[cfg(test)]
mod tests;

use std::path::Path;

use super::{Profile, ProfileFileError, ReadProfile, StackView, query};
use crate::analysis::Granularity;

/// A file format a profile can be read from or written to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    /// microscope's own, which keeps everything.
    Usprof,

    /// pprof's `profile.proto`, gzipped.
    Pprof,

    /// Folded stacks.
    Folded,

    /// Chrome's trace event JSON, written only.
    Chrome,
}

impl Format {
    pub const ALL: [Self; 4] = [Self::Usprof, Self::Pprof, Self::Folded, Self::Chrome];

    /// How commands name it.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Usprof => "usprof",
            Self::Pprof => "pprof",
            Self::Folded => "folded",
            Self::Chrome => "chrome",
        }
    }

    /// The format a file name's extension names, if any.
    #[must_use]
    pub fn of_path(path: &Path) -> Option<Self> {
        let name = path.file_name()?.to_str()?;
        [
            (".usprof", Self::Usprof),
            (".pb.gz", Self::Pprof),
            (".pprof", Self::Pprof),
            (".pb", Self::Pprof),
            (".prof", Self::Pprof),
            (".folded", Self::Folded),
            (".json", Self::Chrome),
        ]
        .into_iter()
        .find_map(|(extension, format)| name.ends_with(extension).then_some(format))
    }
}

impl Format {
    /// The format a file is in, by its first bytes or else its name, if
    /// microscope reads it: `None` for a program, say.
    #[must_use]
    pub fn of_file(path: &Path, head: &[u8]) -> Option<Self> {
        if Profile::is_profile(head) {
            return Some(Self::Usprof);
        }

        if pprof::is_gzip(head) {
            return Some(Self::Pprof);
        }

        Self::of_path(path).filter(|format| *format != Self::Chrome)
    }
}

impl std::str::FromStr for Format {
    type Err = String;

    fn from_str(name: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|format| format.name() == name)
            .ok_or_else(|| format!("no format {name:?}: choose usprof, pprof, folded, or chrome"))
    }
}

impl Profile {
    /// Reads a profile in any format it can be read from: its own, pprof,
    /// gzipped or not, or folded stacks, which is any text.
    pub fn import(bytes: &[u8]) -> Result<ReadProfile, ProfileFileError> {
        if Self::is_profile(bytes) {
            return Self::from_bytes(bytes);
        }

        let capture = match std::str::from_utf8(bytes) {
            Ok(text) if !pprof::is_gzip(bytes) => folded::import(text).map_err(|error| {
                format!("not a profile microscope reads as folded stacks: {error}")
            }),
            _ => pprof::import(bytes).map_err(|error| format!("not a pprof profile: {error}")),
        }
        .map_err(ProfileFileError::Invalid)?;

        Ok(ReadProfile {
            profile: Self { capture },
            truncated: false,
            chunks: 1,
        })
    }

    /// The profile in `format`, its stacks as `view` shows them.
    pub fn export(&self, format: Format, view: StackView) -> std::io::Result<Vec<u8>> {
        let unit = self.setting("weight").unwrap_or("nanoseconds");
        match format {
            Format::Usprof => self.to_bytes(),
            Format::Pprof => {
                let keys = self.keys(Granularity::Line);
                let tree = keys.aggregate(&query(view));
                let (start, end) = self.span();
                pprof::export(&self.capture, &keys, &tree, unit, end - start)
            }
            Format::Folded => {
                let keys = self.keys(Granularity::Function);
                Ok(folded::export(&keys, &keys.aggregate(&query(view))).into_bytes())
            }
            Format::Chrome => Ok(chrome::export(self).into_bytes()),
        }
    }

    /// Whether the profile's samples carry the times they were taken: an
    /// imported pprof or folded profile's do not.
    #[must_use]
    pub fn timed(&self) -> bool {
        self.setting("sample times") != Some("none")
    }
}
