use std::path::{Path, PathBuf};

use crate::{Error, Result};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SourcePathMap {
    rules: Vec<(PathBuf, PathBuf)>,
}

impl SourcePathMap {
    #[must_use]
    pub const fn new() -> Self {
        Self { rules: Vec::new() }
    }

    pub fn push(&mut self, from: impl Into<PathBuf>, to: impl Into<PathBuf>) -> Result<()> {
        let from = from.into();

        if from.components().next().is_none() {
            return Err(Error::EmptySourcePathPrefix);
        }

        self.rules.push((from, to.into()));
        Ok(())
    }

    #[must_use]
    pub fn candidates(&self, recorded: &Path) -> Vec<PathBuf> {
        let bare = recorded.strip_prefix(".").unwrap_or(recorded);
        let rewrites = self.rules.iter().filter_map(|(from, to)| {
            let rest = recorded
                .strip_prefix(from)
                .or_else(|_| bare.strip_prefix(from))
                .ok()?;
            Some(to.join(rest))
        });

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
    fn relative_prefixes_match_whole_components_without_collapsing_dots() {
        let mut map = SourcePathMap::new();
        map.push("build/..", "/tmp/build").unwrap();
        let candidates = |path: &str| map.candidates(Path::new(path));
        assert_eq!(
            candidates("build/../x.c"),
            [Path::new("/tmp/build/x.c"), Path::new("build/../x.c")]
        );
        assert_eq!(candidates("build/x.c"), [Path::new("build/x.c")]);

        assert!(matches!(
            SourcePathMap::new().push("", "/x"),
            Err(Error::EmptySourcePathPrefix)
        ));
    }

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

        let mut map = SourcePathMap::new();
        map.push("a", "").unwrap();
        map.push("a", "a").unwrap();
        map.push("a", "").unwrap();
        assert_eq!(
            map.candidates(Path::new("./a")),
            [Path::new(""), Path::new("a"), Path::new("./a")]
        );
    }

    fn path(absolute: bool, components: &[&str]) -> PathBuf {
        let mut path = PathBuf::from(if absolute { "/" } else { "" });
        path.extend(components);
        path
    }

    fn components() -> impl Strategy<Value = Vec<&'static str>> {
        proptest::collection::vec(prop::sample::select(vec!["a", "b", ".."]), 0..4)
    }

    type Rule = (bool, Vec<&'static str>, bool, Vec<&'static str>);

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
