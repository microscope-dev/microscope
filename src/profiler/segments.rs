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
                "{} has no recording segments",
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

        let capture = builder.finish_read().map_err(|error| {
            ProfileFileError::Invalid(format!(
                "the segments in {} do not merge: {error}",
                directory.display()
            ))
        })?;

        Ok(ReadProfile {
            profile: Self { capture },
            truncated,
            chunks,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture::records::{FrameKind, NONE};
    use crate::model::{Pid, Tid};

    /// Segments that each weigh what a profile may can weigh more
    /// together: the recording is refused, as one segment of that weight
    /// is.
    #[test]
    fn segments_whose_weights_add_up_past_a_count_are_refused() {
        let directory =
            std::env::temp_dir().join(format!("microscope-segments-{}", std::process::id()));
        std::fs::create_dir_all(&directory).expect("a directory");
        let mut builder = CaptureBuilder::new();
        let thread = builder.thread(Pid::new(1), Tid::new(1), 0);
        let frame = builder.frame(FrameKind::Native, NONE, 0x10, 0);
        let stack = builder.stack(&[frame]);
        builder.sample(thread, 1, u64::MAX / 2 + 1, stack, NONE);
        let half = Profile {
            capture: builder.finish(),
        };

        for _ in 0..2 {
            half.write_segment(&directory, Duration::from_secs(3600))
                .expect("a segment");
        }

        let read = Profile::read_segments(&directory);
        std::fs::remove_dir_all(&directory).expect("remove the directory");
        assert!(
            matches!(read, Err(ProfileFileError::Invalid(_))),
            "the merged weight overflows"
        );
    }
}
