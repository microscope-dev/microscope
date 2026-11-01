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
        let rewrites = self.rules.iter().filter_map(|(from, to)| {
            let rest = recorded
                .strip_prefix(from)
                .or_else(|_| bare.strip_prefix(from))
                .ok()?;
            Some(to.join(rest))
        });

        // Rules that rewrite a path alike need not be next to each other.
        let mut candidates = Vec::new();
        for candidate in rewrites {
            if candidate != recorded && !candidates.contains(&candidate) {
                candidates.push(candidate);
            }
        }

        candidates.push(recorded.to_owned());
        candidates
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

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

    /// Two rules that rewrite a path alike, with another rule's rewrite
    /// between them.
    #[test]
    fn a_rewrite_two_rules_make_is_tried_once() {
        let mut map = SourcePathMap::new();
        map.push("/a", "/x").unwrap();
        map.push("/a/b", "/y").unwrap();
        map.push("/a/b", "/x/b").unwrap();
        assert_eq!(
            map.candidates(Path::new("/a/b/f")),
            [Path::new("/x/b/f"), Path::new("/y/f"), Path::new("/a/b/f")]
        );

        // Shrunk by the property below.
        let mut map = SourcePathMap::new();
        map.push("a", "").unwrap();
        map.push("a", "a").unwrap();
        map.push("a", "").unwrap();
        assert_eq!(
            map.candidates(Path::new("./a")),
            [Path::new(""), Path::new("a"), Path::new("./a")]
        );
    }

    /// A path as its components, absolute or not.
    fn path(absolute: bool, components: &[&str]) -> PathBuf {
        let mut path = PathBuf::from(if absolute { "/" } else { "" });
        path.extend(components);
        path
    }

    fn components() -> impl Strategy<Value = Vec<&'static str>> {
        proptest::collection::vec(prop::sample::select(vec!["a", "b", ".."]), 0..4)
    }

    /// A rule, by whether its prefix is absolute, its components, and the
    /// same of what it rewrites to.
    type Rule = (bool, Vec<&'static str>, bool, Vec<&'static str>);

    /// A recorded path's absoluteness and components, and rules most of
    /// which match it, so that their rewrites often repeat.
    fn recorded_and_rules() -> impl Strategy<Value = (bool, Vec<&'static str>, Vec<Rule>)> {
        (any::<bool>(), components()).prop_flat_map(|(absolute, recorded)| {
            let prefix = (0..=recorded.len()).prop_map({
                let recorded = recorded.clone();
                move |length| recorded[..length].to_vec()
            });
            let rule = (
                prop_oneof![3 => Just(absolute), 1 => any::<bool>()],
                prop_oneof![3 => prefix, 1 => components()],
                any::<bool>(),
                components(),
            );
            (
                Just(absolute),
                Just(recorded),
                proptest::collection::vec(rule, 0..6),
            )
        })
    }

    proptest::proptest! {
        /// The candidates are the rewrite of each rule whose prefix is
        /// whole leading components of the path, each once, in the order
        /// of the first rule making it, then the path itself.
        #[test]
        fn candidates_are_each_rewrite_once_then_the_path(
            (absolute, recorded, rules) in recorded_and_rules(),
            dotted in any::<bool>(),
        ) {
            let mut map = SourcePathMap::new();
            let mut expected = Vec::<PathBuf>::new();
            let dotted = dotted && !absolute;
            let recorded_path = if dotted {
                Path::new(".").join(path(false, &recorded))
            } else {
                path(absolute, &recorded)
            };

            for (from_absolute, from, to_absolute, to) in &rules {
                let from_path = path(*from_absolute, from);
                if from.is_empty() && !from_absolute {
                    prop_assert!(map.push(&from_path, path(*to_absolute, to)).is_err());
                    continue;
                }
                map.push(&from_path, path(*to_absolute, to)).unwrap();

                // `/` is a component of its own, which only an absolute
                // path begins with.
                if *from_absolute == absolute && recorded.starts_with(from) {
                    let mut rewrite = path(*to_absolute, to);
                    rewrite.extend(&recorded[from.len()..]);
                    if rewrite != recorded_path && !expected.contains(&rewrite) {
                        expected.push(rewrite);
                    }
                }
            }
            expected.push(recorded_path.clone());

            prop_assert_eq!(map.candidates(&recorded_path), expected);
        }
    }
}
