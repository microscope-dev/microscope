mod builtin;

use std::borrow::Cow;
use std::fmt;
use std::str::FromStr;

use regex::bytes::{Captures, Regex, RegexBuilder};
use serde::{Deserialize, Serialize};

pub use builtin::Builtin;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum FieldClass {
    ProcessName,
    ProcessArgs,
    ThreadName,
    ModulePath,
    SourcePath,
    SyscallPath,
    SyscallString,
    SyscallPayload,
    ZoneText,
    MarkerText,
}

impl FieldClass {
    pub const ALL: [Self; 10] = [
        Self::ProcessName,
        Self::ProcessArgs,
        Self::ThreadName,
        Self::ModulePath,
        Self::SourcePath,
        Self::SyscallPath,
        Self::SyscallString,
        Self::SyscallPayload,
        Self::ZoneText,
        Self::MarkerText,
    ];

    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::ProcessName => "process.name",
            Self::ProcessArgs => "process.args",
            Self::ThreadName => "thread.name",
            Self::ModulePath => "module.path",
            Self::SourcePath => "source.path",
            Self::SyscallPath => "syscall.path",
            Self::SyscallString => "syscall.string",
            Self::SyscallPayload => "syscall.payload",
            Self::ZoneText => "zone.text",
            Self::MarkerText => "marker.text",
        }
    }

    const fn bit(self) -> u16 {
        1 << self as u16
    }
}

impl fmt::Display for FieldClass {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

impl FromStr for FieldClass {
    type Err = RuleError;

    fn from_str(name: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|class| class.name() == name)
            .ok_or_else(|| RuleError::Field(name.to_owned()))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Fields(u16);

impl Fields {
    pub const ALL: Self = Self((1 << FieldClass::ALL.len()) - 1);

    #[must_use]
    pub fn of(classes: &[FieldClass]) -> Self {
        Self(classes.iter().fold(0, |bits, class| bits | class.bit()))
    }

    #[must_use]
    pub const fn contains(self, class: FieldClass) -> bool {
        self.0 & class.bit() != 0
    }

    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Action {
    Redact,
    Hash,
    Mask,
    DropField,
    DropRecord,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuleSpec {
    pub name: String,
    pub fields: Vec<String>,
    #[serde(rename = "match")]
    pub pattern: String,
    pub action: Action,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RuleError {
    #[error("a rule needs a name")]
    Unnamed,

    #[error("{0}: a rule needs at least one field")]
    NoFields(String),

    #[error("no field class {0:?}")]
    Field(String),

    #[error("no built-in rule {0:?}")]
    Builtin(String),

    #[error("{name}: {why}")]
    Pattern { name: String, why: String },
}

const MOST_PATTERN_BYTES: usize = 1 << 20;

#[derive(Debug, Clone)]
pub struct Rule {
    name: String,
    fields: Fields,
    matcher: Matcher,
    action: Action,
}

#[derive(Debug, Clone)]
enum Matcher {
    Pattern(Regex),
    Builtin(Builtin),
}

impl Rule {
    pub fn new(spec: &RuleSpec) -> Result<Self, RuleError> {
        let name = spec.name.trim();
        if name.is_empty() {
            return Err(RuleError::Unnamed);
        }

        let classes = (spec.fields.iter())
            .map(|field| field.parse())
            .collect::<Result<Vec<FieldClass>, _>>()?;
        let fields = Fields::of(&classes);
        if fields.is_empty() {
            return Err(RuleError::NoFields(name.to_owned()));
        }

        let pattern = RegexBuilder::new(&spec.pattern)
            .size_limit(MOST_PATTERN_BYTES)
            .dfa_size_limit(MOST_PATTERN_BYTES)
            .build()
            .map_err(|error| RuleError::Pattern {
                name: name.to_owned(),
                why: error.to_string(),
            })?;

        Ok(Self {
            name: name.to_owned(),
            fields,
            matcher: Matcher::Pattern(pattern),
            action: spec.action,
        })
    }

    #[must_use]
    pub fn builtin(builtin: Builtin) -> Self {
        Self {
            name: builtin.name().to_owned(),
            fields: builtin.fields(),
            matcher: Matcher::Builtin(builtin),
            action: builtin.action(),
        }
    }

    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    fn found(&self, value: &[u8], each: &mut impl FnMut(std::ops::Range<usize>)) {
        match &self.matcher {
            Matcher::Pattern(pattern) => {
                for captures in pattern.captures_iter(value) {
                    each(span(&captures));
                }
            }
            Matcher::Builtin(builtin) => builtin.found(value, each),
        }
    }
}

fn span(captures: &Captures<'_>) -> std::ops::Range<usize> {
    captures
        .name("value")
        .or_else(|| captures.get(0))
        .map_or(0..0, |found| found.range())
}

pub struct HashKey(ring::hmac::Key);

impl HashKey {
    #[must_use]
    pub fn new(secret: &[u8]) -> Self {
        Self(ring::hmac::Key::new(ring::hmac::HMAC_SHA256, secret))
    }

    fn token(&self, value: &[u8]) -> Vec<u8> {
        let tag = ring::hmac::sign(&self.0, value);
        let mut token = b"[hash:".to_vec();
        for byte in tag.as_ref().iter().take(8) {
            token.extend_from_slice(format!("{byte:02x}").as_bytes());
        }

        token.push(b']');
        token
    }
}

impl fmt::Debug for HashKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("HashKey(..)")
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome<T> {
    Kept,
    Replaced(T),
    DropField,
    DropRecord,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Tally(Vec<u64>);

impl Tally {
    fn fired(&mut self, rule: usize, times: u64) {
        if self.0.len() <= rule {
            self.0.resize(rule + 1, 0);
        }

        if let Some(count) = self.0.get_mut(rule) {
            *count += times;
        }
    }

    pub fn merge(&mut self, other: &Self) {
        for (rule, &times) in other.0.iter().enumerate() {
            self.fired(rule, times);
        }
    }
}

#[derive(Debug)]
pub struct Sanitizer {
    rules: Vec<Rule>,
    key: HashKey,
}

pub const REDACTED: &[u8] = b"[redacted]";

impl Sanitizer {
    #[must_use]
    pub fn new(builtins: &[Builtin], custom: Vec<Rule>, key: HashKey) -> Self {
        let mut rules: Vec<Rule> = builtins
            .iter()
            .map(|&builtin| Rule::builtin(builtin))
            .collect();
        rules.extend(custom);
        Self { rules, key }
    }

    pub fn rules(&self) -> impl ExactSizeIterator<Item = &str> {
        self.rules.iter().map(Rule::name)
    }

    pub fn fired<'a>(&'a self, tally: &'a Tally) -> impl Iterator<Item = (&'a str, u64)> {
        (self.rules.iter())
            .zip(tally.0.iter().copied().chain(std::iter::repeat(0)))
            .map(|(rule, times)| (rule.name(), times))
    }

    pub fn bytes(&self, class: FieldClass, value: &[u8], tally: &mut Tally) -> Outcome<Vec<u8>> {
        let mut current: Cow<'_, [u8]> = Cow::Borrowed(value);

        for (index, rule) in self.rules.iter().enumerate() {
            if !rule.fields.contains(class) {
                continue;
            }

            let mut spans = Vec::new();
            rule.found(&current, &mut |found| {
                if !found.is_empty() {
                    spans.push(found);
                }
            });

            if spans.is_empty() {
                continue;
            }

            spans = merged(spans);

            tally.fired(index, spans.len() as u64);
            match rule.action {
                Action::DropRecord => return Outcome::DropRecord,
                Action::DropField => return Outcome::DropField,
                Action::Redact | Action::Hash | Action::Mask => {
                    current = Cow::Owned(self.replaced(&current, &spans, rule.action));
                }
            }
        }

        match current {
            Cow::Borrowed(_) => Outcome::Kept,
            Cow::Owned(changed) => Outcome::Replaced(changed),
        }
    }

    pub fn text(&self, class: FieldClass, value: &str, tally: &mut Tally) -> Outcome<String> {
        match self.bytes(class, value.as_bytes(), tally) {
            Outcome::Kept => Outcome::Kept,
            Outcome::Replaced(changed) => Outcome::Replaced(
                String::from_utf8(changed)
                    .unwrap_or_else(|error| String::from_utf8_lossy(error.as_bytes()).into_owned()),
            ),
            Outcome::DropField => Outcome::DropField,
            Outcome::DropRecord => Outcome::DropRecord,
        }
    }

    pub fn args(&self, args: &[String], tally: &mut Tally) -> Outcome<Vec<String>> {
        match self.text(FieldClass::ProcessArgs, &args.join("\0"), tally) {
            Outcome::Kept => Outcome::Kept,
            Outcome::Replaced(joined) => {
                Outcome::Replaced(joined.split('\0').map(str::to_owned).collect())
            }
            Outcome::DropField => Outcome::DropField,
            Outcome::DropRecord => Outcome::DropRecord,
        }
    }

    fn replaced(&self, value: &[u8], spans: &[std::ops::Range<usize>], action: Action) -> Vec<u8> {
        let mut out = Vec::with_capacity(value.len());
        let mut at = 0;

        for found in spans {
            out.extend_from_slice(value.get(at..found.start).unwrap_or_default());
            let secret = value.get(found.clone()).unwrap_or_default();
            match action {
                Action::Hash => out.extend_from_slice(&self.key.token(secret)),
                Action::Mask => masked(secret, &mut out),
                Action::Redact | Action::DropField | Action::DropRecord => {
                    out.extend_from_slice(REDACTED);
                }
            }

            at = found.end;
        }

        out.extend_from_slice(value.get(at..).unwrap_or_default());
        out
    }
}

fn merged(mut spans: Vec<std::ops::Range<usize>>) -> Vec<std::ops::Range<usize>> {
    spans.sort_by_key(|found| found.start);
    let mut out: Vec<std::ops::Range<usize>> = Vec::with_capacity(spans.len());
    for found in spans {
        match out.last_mut() {
            Some(last) if found.start < last.end => last.end = last.end.max(found.end),
            _ => out.push(found),
        }
    }

    out
}

const MASK_SHOWS: usize = 4;

const MASK_SHOWS_FROM: usize = 12;

fn masked(secret: &[u8], out: &mut Vec<u8>) {
    let shown = if secret.len() >= MASK_SHOWS_FROM {
        MASK_SHOWS
    } else {
        0
    };

    let (hidden, kept) = secret.split_at(secret.len() - shown);
    let hidden = String::from_utf8_lossy(hidden).chars().count();
    out.extend(std::iter::repeat_n(b'*', hidden));
    out.extend_from_slice(kept);
}

#[cfg(test)]
mod tests;
