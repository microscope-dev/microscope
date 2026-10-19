//! A continuous recording: a directory of segments, each a complete
//! profile of its span, named by when it ended, the oldest deleted as
//! they pass the time kept. The directory reads as one profile.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use super::{Profile, ProfileFileError, ReadProfile};
use crate::capture::CaptureBuilder;

/// The extension of a segment, whose name is the milliseconds since the
/// Unix epoch at which it ended.
const EXTENSION: &str = "usprof";

/// When a segment's file name says it ended, if it names a segment.
fn ended(path: &Path) -> Option<u64> {
    (path.extension()? == EXTENSION)
        .then_some(())
        .and_then(|()| path.file_stem()?.to_str()?.parse().ok())
}

/// Every segment in `directory`, oldest first.
fn segments(directory: &Path) -> std::io::Result<Vec<(u64, PathBuf)>> {
    let mut segments = Vec::new();

    for entry in std::fs::read_dir(directory)? {
        let path = entry?.path();
        if let Some(ended) = ended(&path) {
            segments.push((ended, path));
        }
    }

    segments.sort_unstable();
    Ok(segments)
}

fn milliseconds(time: SystemTime) -> u64 {
    time.duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |since| {
            u64::try_from(since.as_millis()).unwrap_or(u64::MAX)
        })
}

impl Profile {
    /// Writes the profile to `directory` as the segment that ends now,
    /// whole or not at all, and deletes each segment that ended more than
    /// `keep` ago. Returns where it wrote the segment.
    pub fn write_segment(&self, directory: &Path, keep: Duration) -> std::io::Result<PathBuf> {
        let now = SystemTime::now();
        let ended = milliseconds(now);

        // Two segments within a millisecond take the next free name.
        let mut name = ended;
        let path = loop {
            let path = directory.join(format!("{name:016}.{EXTENSION}"));
            if !path.try_exists()? {
                break path;
            }

            name += 1;
        };

        let partial = directory.join(format!(".{name:016}.{EXTENSION}.partial"));
        self.write(&partial)?;
        std::fs::rename(&partial, &path)?;

        let oldest = milliseconds(now.checked_sub(keep).unwrap_or(SystemTime::UNIX_EPOCH));
        for (ended, old) in segments(directory)? {
            if ended < oldest {
                std::fs::remove_file(old)?;
            }
        }

        Ok(path)
    }

    /// Reads every segment of a continuous recording in `directory` as one
    /// profile.
    pub fn read_segments(directory: &Path) -> Result<ReadProfile, ProfileFileError> {
        let segments = segments(directory)?;
        if segments.is_empty() {
            return Err(ProfileFileError::Invalid(format!(
                "{} holds no segments of a recording",
                directory.display()
            )));
        }

        let mut builder = CaptureBuilder::new();
        let mut truncated = false;
        let mut chunks = 0;

        for (_, path) in &segments {
            let read = Self::from_bytes(&std::fs::read(path)?).map_err(|error| {
                ProfileFileError::Invalid(format!("{}: {error}", path.display()))
            })?;
            builder.absorb(&read.profile.capture);
            truncated |= read.truncated;
            chunks += read.chunks;
        }

        Ok(ReadProfile {
            profile: Self {
                capture: builder.finish(),
            },
            truncated,
            chunks,
        })
    }
}
