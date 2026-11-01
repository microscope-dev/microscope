//! The source files steps pass through without stopping, as globs over
//! their paths: `**/node_modules/**`, `/srv/vendor/*.js`.
//!
//! `*` and `?` match within one path component and `**` across any
//! number. A pattern that does not begin with `/` matches the end of a
//! path at a component boundary, as a relative breakpoint path does.

use std::sync::Arc;

use crate::{Error, Result};

/// The most patterns kept, and the longest one, in bytes.
const MAX_PATTERNS: usize = 256;
const MAX_PATTERN_BYTES: usize = 1024;

/// A parsed set of skip patterns.
#[derive(Debug, Clone, Default)]
pub struct StepSkips {
    patterns: Arc<[Arc<str>]>,
    matcher: Option<regex::RegexSet>,
}

impl PartialEq for StepSkips {
    fn eq(&self, other: &Self) -> bool {
        self.patterns == other.patterns
    }
}

impl Eq for StepSkips {}

impl StepSkips {
    /// Parses `patterns`, refusing an empty or overlong one.
    pub fn parse<S: AsRef<str>>(patterns: &[S]) -> Result<Self> {
        if patterns.len() > MAX_PATTERNS {
            return Err(Error::InvalidStepSkip(format!(
                "{} patterns, more than {MAX_PATTERNS}",
                patterns.len()
            )));
        }
        let mut expressions = Vec::new();
        for pattern in patterns {
            let pattern = pattern.as_ref();
            if pattern.is_empty() || pattern.len() > MAX_PATTERN_BYTES {
                return Err(Error::InvalidStepSkip(format!(
                    "{pattern:?} must have 1 to {MAX_PATTERN_BYTES} bytes"
                )));
            }
            expressions.push(expression(pattern));
        }
        let matcher = if expressions.is_empty() {
            None
        } else {
            Some(
                regex::RegexSet::new(&expressions)
                    .map_err(|error| Error::InvalidStepSkip(error.to_string()))?,
            )
        };
        Ok(Self {
            patterns: patterns
                .iter()
                .map(|pattern| pattern.as_ref().into())
                .collect(),
            matcher,
        })
    }

    /// The patterns as given.
    #[must_use]
    pub fn patterns(&self) -> &[Arc<str>] {
        &self.patterns
    }

    /// Whether a step passes through code of the file at `path`.
    #[must_use]
    pub fn matches(&self, path: &str) -> bool {
        self.matcher
            .as_ref()
            .is_some_and(|matcher| matcher.is_match(path))
    }
}

/// The regular expression a glob stands for.
fn expression(glob: &str) -> String {
    // `s` lets `**` cross a newline in a file's name, as `*` does.
    let mut expression = String::from(if glob.starts_with('/') {
        "(?s)^"
    } else {
        "(?s)(?:^|/)"
    });
    let mut characters = glob.chars().peekable();
    while let Some(character) = characters.next() {
        match character {
            '*' if characters.peek() == Some(&'*') => {
                characters.next();
                // `**/` also matches no directory at all.
                if characters.peek() == Some(&'/') {
                    characters.next();
                    expression.push_str("(?:.*/)?");
                } else {
                    expression.push_str(".*");
                }
            }
            '*' => expression.push_str("[^/]*"),
            '?' => expression.push_str("[^/]"),
            character => expression.push_str(&regex::escape(&character.to_string())),
        }
    }
    expression.push('$');
    expression
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::StepSkips;

    /// Each pattern, with paths it must match and paths it must not, where
    /// the wrong readings of a glob part from the right one.
    #[test]
    fn globs_match_whole_components() {
        let cases: [(&str, &[&str], &[&str]); 6] = [
            (
                "**/node_modules/**",
                &["/app/node_modules/x/index.js", "node_modules/a.js"],
                &["/app/my_node_modules/a.js", "/app/node_modules"],
            ),
            (
                "vendor/*.js",
                &["/srv/vendor/a.js", "vendor/b.js"],
                &[
                    "/srv/vendor/sub/a.js",
                    "/srv/xvendor/a.js",
                    "/srv/vendor/a.jsx",
                ],
            ),
            (
                "/srv/*.js",
                &["/srv/a.js"],
                &["/other/srv/a.js", "/srv/a/b.js"],
            ),
            (
                "a?.js",
                &["/x/ab.js"],
                &["/x/a.js", "/x/a/.js", "/x/abc.js"],
            ),
            ("lib.min.js", &["/x/lib.min.js"], &["/x/libXminXjs"]),
            ("**/*.mjs", &["/a.mjs", "/a/b/c.mjs"], &["/a/b.js"]),
        ];
        for (pattern, matching, other) in cases {
            let skips = StepSkips::parse(&[pattern]).expect("a pattern");
            for path in matching {
                assert!(skips.matches(path), "{pattern} should match {path}");
            }
            for path in other {
                assert!(!skips.matches(path), "{pattern} should not match {path}");
            }
        }
        assert!(!StepSkips::default().matches("/a.js"));
        assert!(StepSkips::parse(&[""]).is_err());
    }

    /// Shrunk by the property below: `**` stopped at a newline.
    #[test]
    fn stars_cross_a_newline_in_a_name() {
        let skips = StepSkips::parse(&["a**"]).expect("a pattern");
        assert!(skips.matches("/a.\n"));
        assert!(skips.matches("/a/b\nc/d"));
    }

    /// One part of a glob.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Part {
        Literal(char),
        Star,
        Question,
        Stars,
        StarsSlash,
    }

    fn glob(parts: &[Part]) -> String {
        parts
            .iter()
            .map(|part| match part {
                Part::Literal(character) => character.to_string(),
                Part::Star => "*".into(),
                Part::Question => "?".into(),
                Part::Stars => "**".into(),
                Part::StarsSlash => "**/".into(),
            })
            .collect()
    }

    /// The parts of a glob, read from the left: `**/`, `**`, `*`, `?`, or
    /// a character.
    fn tokens(glob: &str) -> Vec<Part> {
        let mut parts = Vec::new();
        let mut rest = glob;
        while let Some(character) = rest.chars().next() {
            let (part, length) = if rest.starts_with("**/") {
                (Part::StarsSlash, 3)
            } else if rest.starts_with("**") {
                (Part::Stars, 2)
            } else if character == '*' {
                (Part::Star, 1)
            } else if character == '?' {
                (Part::Question, 1)
            } else {
                (Part::Literal(character), character.len_utf8())
            };
            parts.push(part);
            rest = &rest[length..];
        }
        parts
    }

    /// Whether `parts` match all of `path`, by the module's rules, trying
    /// every way each wildcard can match.
    fn naive(parts: &[Part], path: &[char]) -> bool {
        let Some((first, rest)) = parts.split_first() else {
            return path.is_empty();
        };

        match first {
            Part::Literal(character) => path.first() == Some(character) && naive(rest, &path[1..]),
            Part::Question => path.first().is_some_and(|c| *c != '/') && naive(rest, &path[1..]),
            Part::Star => (0..=path.len())
                .take_while(|end| !path[..*end].contains(&'/'))
                .any(|end| naive(rest, &path[end..])),
            Part::Stars => (0..=path.len()).any(|end| naive(rest, &path[end..])),
            Part::StarsSlash => {
                naive(rest, path)
                    || (1..=path.len()).any(|end| path[end - 1] == '/' && naive(rest, &path[end..]))
            }
        }
    }

    /// Whether a pattern matches a path: all of it from `/`, or else its
    /// end from a component boundary.
    fn naive_matches(parts: &[Part], path: &str) -> bool {
        let path = path.chars().collect::<Vec<_>>();
        if parts.first() == Some(&Part::Literal('/')) {
            return naive(parts, &path);
        }

        (0..=path.len())
            .filter(|start| *start == 0 || path[start - 1] == '/')
            .any(|start| naive(parts, &path[start..]))
    }

    fn characters() -> impl Strategy<Value = char> {
        prop_oneof![
            8 => prop::sample::select(vec!['a', 'b', '/', '.']),
            1 => Just('\n'),
        ]
    }

    fn parts() -> impl Strategy<Value = Vec<Part>> {
        proptest::collection::vec(
            prop_oneof![
                6 => characters().prop_map(Part::Literal),
                1 => Just(Part::Star),
                1 => Just(Part::Question),
                1 => Just(Part::Stars),
                1 => Just(Part::StarsSlash),
            ],
            1..7,
        )
        .prop_map(|parts| tokens(&glob(&parts)))
    }

    /// A path the parts match, choosing what each wildcard matches.
    fn instance(parts: Vec<Part>) -> impl Strategy<Value = String> {
        let pieces = parts
            .into_iter()
            .map(|part| {
                let text = |range| proptest::collection::vec(characters(), range);
                match part {
                    Part::Literal(character) => Just(character.to_string()).boxed(),
                    Part::Question => prop::sample::select(vec!['a', '.', '\n'])
                        .prop_map(String::from)
                        .boxed(),
                    Part::Star => text(0..3)
                        .prop_map(|text| text.into_iter().filter(|c| *c != '/').collect())
                        .boxed(),
                    Part::Stars => text(0..4).prop_map(String::from_iter).boxed(),
                    Part::StarsSlash => proptest::option::of(text(0..3))
                        .prop_map(|text| {
                            text.map_or_else(String::new, |text| {
                                format!("{}/", String::from_iter(text))
                            })
                        })
                        .boxed(),
                }
            })
            .collect::<Vec<_>>();
        pieces.prop_map(|pieces| pieces.concat())
    }

    proptest::proptest! {
        /// A path made from the pattern matches it, with any directories
        /// before it when the pattern is relative; any other path matches
        /// as the naive reading of the rules says.
        #[test]
        fn globs_match_as_their_rules_say(
            (parts, made, other) in parts().prop_flat_map(|parts| {
                let made = instance(parts.clone());
                let other = proptest::collection::vec(characters(), 0..8)
                    .prop_map(String::from_iter);
                (Just(parts), made, other)
            }),
            directories in proptest::collection::vec(characters(), 0..4),
        ) {
            let pattern = glob(&parts);
            let skips = StepSkips::parse(&[&pattern]).expect("a pattern");
            let made = if pattern.starts_with('/') {
                made
            } else {
                format!("{}/{made}", String::from_iter(directories))
            };

            prop_assert!(skips.matches(&made), "{:?} should match {:?}", pattern, made);
            prop_assert_eq!(
                skips.matches(&other),
                naive_matches(&parts, &other),
                "{:?} on {:?}",
                pattern,
                other
            );
        }
    }
}
