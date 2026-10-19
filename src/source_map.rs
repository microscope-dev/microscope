//! Rewriting source paths recorded by debug information to local paths.

use std::path::{Path, PathBuf};

use crate::{Error, Result};

/// Ordered rules that rewrite recorded source paths, such as those of a
/// program built on another machine or in a container, to where the files
/// are on this machine.
///
/// A rule applies to a recorded path that begins with all of its prefix's
/// components, so `/work` rewrites `/work/main.c` but never
/// `/workspace/main.c`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SourcePathMap {
    rules: Vec<(PathBuf, PathBuf)>,
}

impl SourcePathMap {
    /// Creates a map with no rules, which reads every recorded path as is.
    #[must_use]
    pub const fn new() -> Self {
        Self { rules: Vec::new() }
    }

    /// Adds a rule after every existing one: recorded paths below `from` are
    /// looked for below `to`.
    pub fn push(&mut self, from: impl Into<PathBuf>, to: impl Into<PathBuf>) -> Result<()> {
        let from = from.into();

        // An empty prefix would claim every path.
        if from.components().next().is_none() {
            return Err(Error::EmptySourcePathPrefix);
        }

        self.rules.push((from, to.into()));
        Ok(())
    }

    /// Where a recorded source file may be, in the order to try: the rewrite
    /// of each matching rule, then the recorded path itself.
    #[must_use]
    pub fn candidates(&self, recorded: &Path) -> Vec<PathBuf> {
        // A path recorded relative to `.`, as `-trimpath` builds record
        // theirs, matches a rule with or without the leading `./`.
        let bare = recorded.strip_prefix(".").unwrap_or(recorded);
        let mut candidates = self
            .rules
            .iter()
            .filter_map(|(from, to)| {
                let rest = recorded
                    .strip_prefix(from)
                    .or_else(|_| bare.strip_prefix(from))
                    .ok()?;
                Some(to.join(rest))
            })
            .filter(|candidate| candidate != recorded)
            .collect::<Vec<_>>();

        candidates.push(recorded.to_owned());
        candidates.dedup();
        candidates
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rules_rewrite_whole_leading_components_in_order() {
        let mut map = SourcePathMap::new();
        map.push("/work", "/home/me/src").unwrap();
        map.push("/work/vendor", "/opt/vendor").unwrap();
        map.push("build/..", "/tmp/build").unwrap();
        let candidates = |path: &str| map.candidates(Path::new(path));

        assert_eq!(
            candidates("/work/vendor/lib.c"),
            [
                Path::new("/home/me/src/vendor/lib.c"),
                Path::new("/opt/vendor/lib.c"),
                Path::new("/work/vendor/lib.c"),
            ]
        );
        assert_eq!(
            candidates("/work"),
            [Path::new("/home/me/src"), Path::new("/work")]
        );
        assert_eq!(candidates("/workspace/a.c"), [Path::new("/workspace/a.c")]);
        assert_eq!(candidates("/a/work/b.c"), [Path::new("/a/work/b.c")]);

        // Relative recorded paths match relative prefixes component by
        // component; `..` is not collapsed.
        assert_eq!(
            candidates("build/../x.c"),
            [Path::new("/tmp/build/x.c"), Path::new("build/../x.c")]
        );
        assert_eq!(candidates("build/x.c"), [Path::new("build/x.c")]);

        // A rule mapping a prefix to itself does not repeat the path, which
        // stays last.
        let mut identity = SourcePathMap::new();
        identity.push("/src", "/src").unwrap();
        assert_eq!(
            identity.candidates(Path::new("/src/a.c")),
            [Path::new("/src/a.c")]
        );
        identity.push("/src", "/tmp").unwrap();
        assert_eq!(
            identity.candidates(Path::new("/src/a.c")),
            [Path::new("/tmp/a.c"), Path::new("/src/a.c")]
        );
        assert!(matches!(
            SourcePathMap::new().push("", "/x"),
            Err(Error::EmptySourcePathPrefix)
        ));
    }
}
