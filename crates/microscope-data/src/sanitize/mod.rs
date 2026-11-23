mod builtin;
pub mod chunk;

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::fmt;
use std::str::FromStr;

use regex::bytes::{Captures, Regex, RegexBuilder};
use serde::{Deserialize, Serialize};

pub use builtin::Builtin;
pub use chunk::{
    EncodedPost, SanitizedCapture, SanitizedChunk, decode_post_metadata, encode_post,
    re_sanitize_sections_verbatim, sanitize_capture, sanitize_chunk,
};

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

    #[must_use]
    pub const fn number(self) -> u16 {
        self as u16
    }

    #[must_use]
    pub const fn from_number(number: u16) -> Option<Self> {
        match number {
            0 => Some(Self::ProcessName),
            1 => Some(Self::ProcessArgs),
            2 => Some(Self::ThreadName),
            3 => Some(Self::ModulePath),
            4 => Some(Self::SourcePath),
            5 => Some(Self::SyscallPath),
            6 => Some(Self::SyscallString),
            7 => Some(Self::SyscallPayload),
            8 => Some(Self::ZoneText),
            9 => Some(Self::MarkerText),
            _ => None,
        }
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

impl Action {
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "redact" => Some(Self::Redact),
            "hash" => Some(Self::Hash),
            "mask" => Some(Self::Mask),
            "drop-field" | "drop_field" => Some(Self::DropField),
            "drop-record" | "drop_record" => Some(Self::DropRecord),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PolicySanitizerError {
    #[error("invalid sanitizer hash key")]
    InvalidHashKey,

    #[error("unknown sanitizer action {0:?}")]
    UnknownAction(String),

    #[error(transparent)]
    Rule(#[from] RuleError),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PolicyRuleInput<'a> {
    pub name: &'a str,
    pub fields: &'a [String],
    pub pattern: &'a str,
    pub action: &'a str,
}

#[expect(
    clippy::struct_excessive_bools,
    reason = "maps 1:1 to the four optional built-in sanitizer toggles"
)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PolicyBuiltins {
    pub emails: bool,
    pub ip_addresses: bool,
    pub home_directories: bool,
    pub sql_literals: bool,
}

impl PolicyBuiltins {
    #[must_use]
    pub fn to_vec(self) -> Vec<Builtin> {
        let mut builtins = Builtin::DEFAULT.to_vec();
        if self.emails {
            builtins.push(Builtin::Emails);
        }
        if self.ip_addresses {
            builtins.push(Builtin::IpAddresses);
        }
        if self.home_directories {
            builtins.push(Builtin::HomeDirectories);
        }
        if self.sql_literals {
            builtins.push(Builtin::SqlLiterals);
        }
        builtins
    }
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
pub struct Tally {
    totals: Vec<u64>,
    by_field: BTreeMap<(usize, FieldClass), u64>,
}

impl Tally {
    fn fired(&mut self, rule: usize, class: FieldClass, times: u64) {
        if self.totals.len() <= rule {
            self.totals.resize(rule + 1, 0);
        }

        if let Some(count) = self.totals.get_mut(rule) {
            *count = count.saturating_add(times);
        }

        if times > 0 {
            let entry = self.by_field.entry((rule, class)).or_default();
            *entry = entry.saturating_add(times);
        }
    }

    pub fn merge(&mut self, other: &Self) {
        for (&(rule, class), &times) in &other.by_field {
            self.fired(rule, class, times);
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

    pub fn from_policy(
        builtins: PolicyBuiltins,
        custom_rules: &[PolicyRuleInput<'_>],
        hash_key: HashKey,
    ) -> Result<Self, PolicySanitizerError> {
        let mut compiled = Vec::with_capacity(custom_rules.len());
        for raw in custom_rules {
            let action = Action::parse(raw.action)
                .ok_or_else(|| PolicySanitizerError::UnknownAction(raw.action.to_owned()))?;
            let spec = RuleSpec {
                name: raw.name.to_owned(),
                fields: raw.fields.to_vec(),
                pattern: raw.pattern.to_owned(),
                action,
            };
            compiled.push(Rule::new(&spec)?);
        }
        Ok(Self::new(&builtins.to_vec(), compiled, hash_key))
    }

    pub fn rules(&self) -> impl ExactSizeIterator<Item = &str> {
        self.rules.iter().map(Rule::name)
    }

    pub fn fired<'a>(&'a self, tally: &'a Tally) -> impl Iterator<Item = (&'a str, u64)> {
        (self.rules.iter())
            .zip(tally.totals.iter().copied().chain(std::iter::repeat(0)))
            .map(|(rule, times)| (rule.name(), times))
    }

    pub fn fired_by_field<'a>(
        &'a self,
        tally: &'a Tally,
    ) -> impl Iterator<Item = (&'a str, FieldClass, u64)> {
        tally
            .by_field
            .iter()
            .filter_map(|(&(rule, class), &times)| {
                self.rules.get(rule).map(|rule| (rule.name(), class, times))
            })
    }

    pub fn bytes(&self, class: FieldClass, value: &[u8], tally: &mut Tally) -> Outcome<Vec<u8>> {
        self.bytes_inner(class, value, tally, false)
    }

    fn bytes_inner(
        &self,
        class: FieldClass,
        value: &[u8],
        tally: &mut Tally,
        skip_secret_flags: bool,
    ) -> Outcome<Vec<u8>> {
        let mut current: Cow<'_, [u8]> = Cow::Borrowed(value);

        for (index, rule) in self.rules.iter().enumerate() {
            if !rule.fields.contains(class) {
                continue;
            }
            if skip_secret_flags && matches!(rule.matcher, Matcher::Builtin(Builtin::SecretFlags)) {
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
            if matches!(rule.action, Action::Redact | Action::Hash | Action::Mask) {
                let protected = sanitized_token_ranges(&current);
                if !protected.is_empty() {
                    spans = subtract_ranges(spans, &protected);
                }
                if rule.action == Action::Mask {
                    let masked_ranges = masked_token_ranges(&current);
                    if !masked_ranges.is_empty() {
                        spans = subtract_ranges(spans, &masked_ranges);
                    }
                }
                if spans.is_empty() {
                    continue;
                }
            }

            match rule.action {
                Action::DropRecord => {
                    tally.fired(index, class, spans.len() as u64);
                    return Outcome::DropRecord;
                }
                Action::DropField => {
                    tally.fired(index, class, spans.len() as u64);
                    return Outcome::DropField;
                }
                Action::Redact | Action::Hash | Action::Mask => {
                    let replaced = self.replaced(&current, &spans, rule.action);
                    if replaced != current.as_ref() {
                        tally.fired(index, class, spans.len() as u64);
                        current = Cow::Owned(replaced);
                    }
                }
            }
        }

        match current {
            Cow::Borrowed(_) => Outcome::Kept,
            Cow::Owned(changed) => Outcome::Replaced(changed),
        }
    }

    pub fn text(&self, class: FieldClass, value: &str, tally: &mut Tally) -> Outcome<String> {
        self.text_inner(class, value, tally, false)
    }

    fn text_inner(
        &self,
        class: FieldClass,
        value: &str,
        tally: &mut Tally,
        skip_secret_flags: bool,
    ) -> Outcome<String> {
        match self.bytes_inner(class, value.as_bytes(), tally, skip_secret_flags) {
            Outcome::Kept => Outcome::Kept,
            Outcome::Replaced(changed) => Outcome::Replaced(
                String::from_utf8(changed)
                    .unwrap_or_else(|error| String::from_utf8_lossy(error.as_bytes()).into_owned()),
            ),
            Outcome::DropField => Outcome::DropField,
            Outcome::DropRecord => Outcome::DropRecord,
        }
    }

    #[expect(
        clippy::too_many_lines,
        reason = "redacts structured flag values, embedded flags, and cross-argument rules in one pass"
    )]
    pub fn args(&self, args: &[String], tally: &mut Tally) -> Outcome<Vec<String>> {
        let mut working: Vec<String> = args.to_vec();
        let mut modified = false;
        let mut handled = vec![false; working.len()];

        for (index, rule) in self.rules.iter().enumerate() {
            if !rule.fields.contains(FieldClass::ProcessArgs) {
                continue;
            }
            if !matches!(rule.matcher, Matcher::Builtin(Builtin::SecretFlags)) {
                continue;
            }

            let mut redact_next = false;
            for (arg, was_handled) in working.iter_mut().zip(&mut handled) {
                if redact_next {
                    redact_next = false;
                    *was_handled = true;
                    if !arg.is_empty() && !is_sanitized_token(arg.as_bytes()) {
                        match rule.action {
                            Action::DropRecord => {
                                tally.fired(index, FieldClass::ProcessArgs, 1);
                                return Outcome::DropRecord;
                            }
                            Action::DropField => {
                                tally.fired(index, FieldClass::ProcessArgs, 1);
                                return Outcome::DropField;
                            }
                            Action::Redact | Action::Hash | Action::Mask => {
                                let span = 0..arg.len();
                                let replaced = self.replaced(
                                    arg.as_bytes(),
                                    std::slice::from_ref(&span),
                                    rule.action,
                                );
                                if replaced != arg.as_bytes() {
                                    tally.fired(index, FieldClass::ProcessArgs, 1);
                                    *arg = String::from_utf8(replaced).unwrap_or_else(|error| {
                                        String::from_utf8_lossy(error.as_bytes()).into_owned()
                                    });
                                    modified = true;
                                }
                            }
                        }
                    }
                    continue;
                }

                match builtin::secret_flag_arg(arg) {
                    builtin::SecretFlagArg::None => {}
                    builtin::SecretFlagArg::NextArg => {
                        redact_next = true;
                    }
                    builtin::SecretFlagArg::Value(span) => {
                        *was_handled = true;
                        let val_bytes = arg.as_bytes().get(span.clone()).unwrap_or_default();
                        if !is_sanitized_token(val_bytes) {
                            match rule.action {
                                Action::DropRecord => {
                                    tally.fired(index, FieldClass::ProcessArgs, 1);
                                    return Outcome::DropRecord;
                                }
                                Action::DropField => {
                                    tally.fired(index, FieldClass::ProcessArgs, 1);
                                    return Outcome::DropField;
                                }
                                Action::Redact | Action::Hash | Action::Mask => {
                                    let replaced = self.replaced(
                                        arg.as_bytes(),
                                        std::slice::from_ref(&span),
                                        rule.action,
                                    );
                                    if replaced != arg.as_bytes() {
                                        tally.fired(index, FieldClass::ProcessArgs, 1);
                                        *arg =
                                            String::from_utf8(replaced).unwrap_or_else(|error| {
                                                String::from_utf8_lossy(error.as_bytes())
                                                    .into_owned()
                                            });
                                        modified = true;
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }

        for (arg, &was_handled) in working.iter_mut().zip(&handled) {
            if was_handled {
                continue;
            }
            for (index, rule) in self.rules.iter().enumerate() {
                if !rule.fields.contains(FieldClass::ProcessArgs)
                    || !matches!(rule.matcher, Matcher::Builtin(Builtin::SecretFlags))
                {
                    continue;
                }
                let mut spans = Vec::new();
                rule.found(arg.as_bytes(), &mut |found| {
                    if !found.is_empty() {
                        spans.push(found);
                    }
                });
                if spans.is_empty() {
                    continue;
                }
                let mut spans = merged(spans);
                if matches!(rule.action, Action::Redact | Action::Hash | Action::Mask) {
                    let protected = sanitized_token_ranges(arg.as_bytes());
                    if !protected.is_empty() {
                        spans = subtract_ranges(spans, &protected);
                    }
                    if rule.action == Action::Mask {
                        let masked_ranges = masked_token_ranges(arg.as_bytes());
                        if !masked_ranges.is_empty() {
                            spans = subtract_ranges(spans, &masked_ranges);
                        }
                    }
                    if spans.is_empty() {
                        continue;
                    }
                }
                match rule.action {
                    Action::DropRecord => {
                        tally.fired(index, FieldClass::ProcessArgs, spans.len() as u64);
                        return Outcome::DropRecord;
                    }
                    Action::DropField => {
                        tally.fired(index, FieldClass::ProcessArgs, spans.len() as u64);
                        return Outcome::DropField;
                    }
                    Action::Redact | Action::Hash | Action::Mask => {
                        let replaced = self.replaced(arg.as_bytes(), &spans, rule.action);
                        if replaced != arg.as_bytes() {
                            tally.fired(index, FieldClass::ProcessArgs, spans.len() as u64);
                            *arg = String::from_utf8(replaced).unwrap_or_else(|error| {
                                String::from_utf8_lossy(error.as_bytes()).into_owned()
                            });
                            modified = true;
                        }
                    }
                }
            }
        }

        if working.iter().any(|arg| arg.as_bytes().contains(&0)) {
            for arg in &mut working {
                match self.text_inner(FieldClass::ProcessArgs, arg, tally, true) {
                    Outcome::Kept => {}
                    Outcome::Replaced(changed) => {
                        *arg = changed;
                        modified = true;
                    }
                    Outcome::DropField => return Outcome::DropField,
                    Outcome::DropRecord => return Outcome::DropRecord,
                }
            }
            return if modified {
                Outcome::Replaced(working)
            } else {
                Outcome::Kept
            };
        }

        let joined = working.join("\0");
        let mut boundaries: Vec<(usize, usize)> = Vec::with_capacity(working.len());
        let mut offset = 0_usize;
        for arg in &working {
            let end = offset + arg.len();
            boundaries.push((offset, end));
            offset = end + 1;
        }

        let mut arg_spans: Vec<Vec<(usize, Action, std::ops::Range<usize>)>> =
            vec![Vec::new(); working.len()];
        for (index, rule) in self.rules.iter().enumerate() {
            if !rule.fields.contains(FieldClass::ProcessArgs)
                || matches!(rule.matcher, Matcher::Builtin(Builtin::SecretFlags))
            {
                continue;
            }
            let mut spans = Vec::new();
            rule.found(joined.as_bytes(), &mut |found| {
                if !found.is_empty() {
                    spans.push(found);
                }
            });
            if spans.is_empty() {
                continue;
            }
            spans = merged(spans);
            match rule.action {
                Action::DropRecord => {
                    tally.fired(index, FieldClass::ProcessArgs, spans.len() as u64);
                    return Outcome::DropRecord;
                }
                Action::DropField => {
                    tally.fired(index, FieldClass::ProcessArgs, spans.len() as u64);
                    return Outcome::DropField;
                }
                Action::Redact | Action::Hash | Action::Mask => {
                    for found in spans {
                        for (arg_idx, &(start, end)) in boundaries.iter().enumerate() {
                            let lo = found.start.max(start);
                            let hi = found.end.min(end);
                            if lo < hi {
                                arg_spans[arg_idx].push((
                                    index,
                                    rule.action,
                                    (lo - start)..(hi - start),
                                ));
                            }
                        }
                    }
                }
            }
        }

        for (arg, spans_for_arg) in working.iter_mut().zip(arg_spans) {
            if spans_for_arg.is_empty() {
                continue;
            }
            let mut by_rule: BTreeMap<usize, (Action, Vec<std::ops::Range<usize>>)> =
                BTreeMap::new();
            for (rule_idx, action, range) in spans_for_arg {
                by_rule
                    .entry(rule_idx)
                    .or_insert_with(|| (action, Vec::new()))
                    .1
                    .push(range);
            }
            for (rule_idx, (action, raw_spans)) in by_rule {
                let mut spans = merged(raw_spans);
                let protected = sanitized_token_ranges(arg.as_bytes());
                if !protected.is_empty() {
                    spans = subtract_ranges(spans, &protected);
                }
                if action == Action::Mask {
                    let masked_ranges = masked_token_ranges(arg.as_bytes());
                    if !masked_ranges.is_empty() {
                        spans = subtract_ranges(spans, &masked_ranges);
                    }
                }
                if spans.is_empty() {
                    continue;
                }
                let replaced = self.replaced(arg.as_bytes(), &spans, action);
                if replaced != arg.as_bytes() {
                    tally.fired(rule_idx, FieldClass::ProcessArgs, spans.len() as u64);
                    *arg = String::from_utf8(replaced).unwrap_or_else(|error| {
                        String::from_utf8_lossy(error.as_bytes()).into_owned()
                    });
                    modified = true;
                }
            }
        }

        if modified {
            Outcome::Replaced(working)
        } else {
            Outcome::Kept
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

const HASH_TOKEN_PREFIX: &[u8] = b"[hash:";
const HASH_TOKEN_LEN: usize = 6 + 16 + 1;

fn is_hash_token_at(bytes: &[u8], start: usize) -> bool {
    let Some(slice) = bytes.get(start..start + HASH_TOKEN_LEN) else {
        return false;
    };
    slice.starts_with(HASH_TOKEN_PREFIX)
        && slice.last() == Some(&b']')
        && slice
            .get(6..22)
            .is_some_and(|hex| hex.iter().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')))
}

fn is_sanitized_token(bytes: &[u8]) -> bool {
    bytes == REDACTED || (bytes.len() == HASH_TOKEN_LEN && is_hash_token_at(bytes, 0))
}

fn sanitized_token_ranges(bytes: &[u8]) -> Vec<std::ops::Range<usize>> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes.get(i) == Some(&b'[') {
            if bytes.get(i..i + REDACTED.len()) == Some(REDACTED) {
                out.push(i..i + REDACTED.len());
                i += REDACTED.len();
                continue;
            }
            if is_hash_token_at(bytes, i) {
                out.push(i..i + HASH_TOKEN_LEN);
                i += HASH_TOKEN_LEN;
                continue;
            }
        }
        i += 1;
    }
    out
}

fn subtract_ranges(
    spans: Vec<std::ops::Range<usize>>,
    excluded: &[std::ops::Range<usize>],
) -> Vec<std::ops::Range<usize>> {
    let mut out = Vec::with_capacity(spans.len());
    for span in spans {
        let mut cur = span.start;
        for ex in excluded {
            if ex.end <= cur {
                continue;
            }
            if ex.start >= span.end {
                break;
            }
            if ex.start > cur {
                out.push(cur..ex.start);
            }
            cur = cur.max(ex.end);
            if cur >= span.end {
                break;
            }
        }
        if cur < span.end {
            out.push(cur..span.end);
        }
    }
    out
}

const MASK_SHOWS: usize = 4;

const MASK_SHOWS_FROM: usize = 12;

const fn is_masked_token_boundary(ch: char) -> bool {
    ch.is_whitespace()
        || matches!(
            ch,
            '\0' | ',' | ';' | '"' | '\'' | '(' | ')' | '[' | ']' | '{' | '}' | '<' | '>' | '&'
        )
}

fn masked_token_ranges(bytes: &[u8]) -> Vec<std::ops::Range<usize>> {
    let min_stars = MASK_SHOWS_FROM - MASK_SHOWS;
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes.get(i) != Some(&b'*') {
            i += 1;
            continue;
        }
        let star_start = i;
        while bytes.get(i) == Some(&b'*') {
            i += 1;
        }
        let star_end = i;
        if star_end - star_start < min_stars {
            continue;
        }
        let Some(tail) = bytes
            .get(star_end..)
            .and_then(|s| std::str::from_utf8(s).ok())
        else {
            continue;
        };
        let mut suffix_bytes = 0;
        let mut suffix_chars = 0;
        for (offset, ch) in tail.char_indices() {
            if is_masked_token_boundary(ch) {
                break;
            }
            suffix_chars += 1;
            suffix_bytes = offset + ch.len_utf8();
            if suffix_chars > MASK_SHOWS {
                break;
            }
        }
        if (1..=MASK_SHOWS).contains(&suffix_chars) {
            out.push(star_start..star_end + suffix_bytes);
            i = star_end + suffix_bytes;
        }
    }
    out
}

fn masked(secret: &[u8], out: &mut Vec<u8>) {
    let text = String::from_utf8_lossy(secret);
    let chars: Vec<char> = text.chars().collect();
    let shown = if chars.len() >= MASK_SHOWS_FROM {
        MASK_SHOWS
    } else {
        0
    };
    let hidden = chars.len() - shown;
    out.extend(std::iter::repeat_n(b'*', hidden));
    for ch in chars.iter().skip(hidden) {
        let mut buf = [0_u8; 4];
        out.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
    }
}

#[cfg(test)]
mod tests;
