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
    let mut expression = String::from(if glob.starts_with('/') {
        "^"
    } else {
        "(?:^|/)"
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
}
