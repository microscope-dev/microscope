//! Built-in secret and PII detectors with fast byte prefilters
//! (`fast_prefilter`, `pool_may_match`) to skip regex evaluation on clean input.

use std::net::{Ipv4Addr, Ipv6Addr};
use std::ops::Range;
use std::str::FromStr;
use std::sync::LazyLock;

use regex::bytes::{Regex, RegexSet};

use super::{Action, FieldClass, Fields, RuleError, span};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Builtin {
    SecretFlags,
    UrlPasswords,
    Tokens,
    Cards,
    Emails,
    IpAddresses,
    HomeDirectories,
    SqlLiterals,
}

impl Builtin {
    pub const ALL: [Self; 8] = [
        Self::SecretFlags,
        Self::UrlPasswords,
        Self::Tokens,
        Self::Cards,
        Self::Emails,
        Self::IpAddresses,
        Self::HomeDirectories,
        Self::SqlLiterals,
    ];

    pub const DEFAULT: [Self; 4] = [
        Self::SecretFlags,
        Self::UrlPasswords,
        Self::Tokens,
        Self::Cards,
    ];

    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::SecretFlags => "secrets in arguments",
            Self::UrlPasswords => "passwords in URLs",
            Self::Tokens => "token shapes",
            Self::Cards => "card numbers",
            Self::Emails => "email addresses",
            Self::IpAddresses => "IP addresses",
            Self::HomeDirectories => "home directory names",
            Self::SqlLiterals => "SQL literals",
        }
    }

    #[must_use]
    pub const fn action(self) -> Action {
        match self {
            Self::SecretFlags | Self::UrlPasswords | Self::Tokens | Self::SqlLiterals => {
                Action::Redact
            }
            Self::Cards => Action::Mask,
            Self::Emails | Self::IpAddresses | Self::HomeDirectories => Action::Hash,
        }
    }

    #[must_use]
    pub fn fields(self) -> Fields {
        use FieldClass::{
            MarkerText, ModulePath, ProcessArgs, SourcePath, SyscallPath, SyscallPayload,
            SyscallString, ZoneText,
        };

        let text = [
            ProcessArgs,
            SyscallPath,
            SyscallString,
            SyscallPayload,
            ZoneText,
            MarkerText,
        ];
        match self {
            Self::SecretFlags
            | Self::UrlPasswords
            | Self::Tokens
            | Self::Cards
            | Self::Emails
            | Self::IpAddresses => Fields::of(&text),
            Self::HomeDirectories => Fields::of(&[
                ProcessArgs,
                ModulePath,
                SourcePath,
                SyscallPath,
                SyscallString,
            ]),
            Self::SqlLiterals => Fields::of(&[ZoneText, MarkerText]),
        }
    }

    #[inline]
    fn fast_prefilter(self, value: &[u8]) -> bool {
        match self {
            Self::SecretFlags => {
                value.len() >= 8
                    && value
                        .iter()
                        .any(|&b| b == b'=' || b == 0 || b.is_ascii_whitespace())
            }
            Self::UrlPasswords => {
                value.len() >= 8
                    && memchr::memchr(b'@', value).is_some()
                    && memchr::memchr(b':', value).is_some()
            }
            Self::Tokens => value.len() >= 14,
            Self::Cards => {
                if value.len() < 13 {
                    return false;
                }
                let mut digits = 0_usize;
                for &b in value {
                    if b.is_ascii_digit() {
                        digits += 1;
                        if digits >= 13 {
                            return true;
                        }
                    }
                }
                false
            }
            Self::Emails => {
                value.len() >= 6
                    && memchr::memchr(b'@', value).is_some()
                    && memchr::memchr(b'.', value).is_some()
            }
            Self::IpAddresses => value.len() >= 2 && memchr::memchr2(b'.', b':', value).is_some(),
            Self::HomeDirectories => value.len() >= 7 && memchr::memchr(b'/', value).is_some(),
            Self::SqlLiterals => value.iter().any(|&b| b == b'\'' || b.is_ascii_digit()),
        }
    }

    pub(super) fn pool_may_match(self, pool: &[u8]) -> bool {
        match self {
            Self::SecretFlags => {
                pool.len() >= 8 && pool.iter().any(|&b| b == b'=' || b.is_ascii_whitespace())
            }
            Self::UrlPasswords => {
                pool.len() >= 8
                    && memchr::memchr(b'@', pool).is_some()
                    && memchr::memchr(b':', pool).is_some()
            }
            Self::Tokens => pool.len() >= 14 && token_set().is_match(pool),
            Self::Cards => {
                let mut run = 0_usize;
                for &b in pool {
                    if b == 0 {
                        run = 0;
                    } else if b.is_ascii_digit() {
                        run += 1;
                        if run >= 13 {
                            return true;
                        }
                    }
                }
                false
            }
            Self::Emails => {
                pool.len() >= 6
                    && memchr::memchr(b'@', pool).is_some()
                    && memchr::memchr(b'.', pool).is_some()
            }
            Self::IpAddresses => pool.len() >= 2 && memchr::memchr2(b'.', b':', pool).is_some(),
            Self::HomeDirectories => pool.len() >= 7 && memchr::memchr(b'/', pool).is_some(),
            Self::SqlLiterals => pool.iter().any(|&b| b == b'\'' || b.is_ascii_digit()),
        }
    }

    pub(super) fn found(self, value: &[u8], each: &mut impl FnMut(Range<usize>)) {
        if !self.fast_prefilter(value) {
            return;
        }
        if self == Self::Tokens {
            let matches = token_set().matches(value);
            if !matches.matched_any() {
                return;
            }
            let pats = patterns(self);
            for idx in &matches {
                if let Some(pattern) = pats.get(idx) {
                    for captures in pattern.captures_iter(value) {
                        each(span(&captures));
                    }
                }
            }
            return;
        }
        for pattern in patterns(self) {
            for captures in pattern.captures_iter(value) {
                let found = span(&captures);
                let ok = match self {
                    Self::Cards => value.get(found.clone()).is_some_and(luhn),
                    Self::IpAddresses => ip_in_context(value, &found),
                    _ => true,
                };
                if ok {
                    each(found);
                }
            }
        }
    }
}

impl FromStr for Builtin {
    type Err = RuleError;

    fn from_str(name: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|builtin| builtin.name() == name)
            .ok_or_else(|| RuleError::Builtin(name.to_owned()))
    }
}

const SECRET_WORDS: &str = r"[a-z0-9_.-]*(?:password|passwd|passphrase|token|api[_-]?key|secret|credentials?|private[_-]?key)[a-z0-9_.-]*";

pub enum SecretFlagArg {
    None,
    NextArg,
    Value(Range<usize>),
}

pub fn secret_flag_arg(arg: &str) -> SecretFlagArg {
    static FLAG_ONLY: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(&format!(r"(?i-u)^--?{SECRET_WORDS}$"))
            .unwrap_or_else(|error| panic!("flag-only regex: {error}"))
    });
    static FLAG_VALUE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(&format!(
            r"(?is-u)^(?:--?{SECRET_WORDS}|[A-Z_]*{SECRET_WORDS})=(?<value>.+)$"
        ))
        .unwrap_or_else(|error| panic!("flag-value regex: {error}"))
    });

    if FLAG_ONLY.is_match(arg.as_bytes()) {
        return SecretFlagArg::NextArg;
    }
    if let Some(captures) = FLAG_VALUE.captures(arg.as_bytes())
        && let Some(value) = captures.name("value")
    {
        return SecretFlagArg::Value(value.range());
    }
    SecretFlagArg::None
}

fn compile(patterns: &[&str]) -> Vec<Regex> {
    patterns
        .iter()
        .map(|pattern| Regex::new(pattern).unwrap_or_else(|error| panic!("{pattern}: {error}")))
        .collect()
}

const TOKEN_PATTERNS: [&str; 7] = [
    r"(?-u)\b(?:AKIA|ASIA)[0-9A-Z]{16}\b",
    r"(?i-u)aws_secret_access_key[\s\x00]*[=:][\s\x00]*(?<value>[A-Za-z0-9/+=]{40})",
    r"(?-u)\b(?:gh[pousr]_[A-Za-z0-9]{36,255}|github_pat_[A-Za-z0-9_]{22,255})\b",
    r"(?-u)\bxox[abposr]-[A-Za-z0-9-]{10,}",
    r"(?-u)\beyJ[A-Za-z0-9_-]{8,}\.eyJ[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}",
    r"(?s-u)-----BEGIN [A-Z ]*PRIVATE KEY-----.*?(?:-----END [A-Z ]*PRIVATE KEY-----|\z)",
    r"(?i-u)\b(?:bearer|basic)[\s\x00]+(?<value>[A-Za-z0-9._~+/=-]{8,})",
];

fn token_set() -> &'static RegexSet {
    static SET: LazyLock<RegexSet> = LazyLock::new(|| {
        RegexSet::new(TOKEN_PATTERNS).unwrap_or_else(|error| panic!("token regex set: {error}"))
    });
    &SET
}

fn patterns(builtin: Builtin) -> &'static [Regex] {
    static SECRET_FLAGS: LazyLock<Vec<Regex>> = LazyLock::new(|| {
        compile(&[
            &format!(r"(?i-u)(?:^|[\s\x00])--?{SECRET_WORDS}(?:=|[\s\x00]+)(?<value>[^\s\x00]+)"),
            &format!(r"(?i-u)(?:^|[\s\x00,;&?])[A-Z_]*{SECRET_WORDS}=(?<value>[^\s\x00&;,]+)"),
        ])
    });
    static URL_PASSWORDS: LazyLock<Vec<Regex>> = LazyLock::new(|| {
        compile(&[r"(?i-u)[a-z][a-z0-9+.-]*://[^/\s\x00:@]*:(?<value>[^/\s\x00@]+)@"])
    });
    static TOKENS: LazyLock<Vec<Regex>> = LazyLock::new(|| compile(&TOKEN_PATTERNS));
    static CARDS: LazyLock<Vec<Regex>> =
        LazyLock::new(|| compile(&[r"(?-u)\b\d(?:[ -]?\d){12,18}\b"]));
    static EMAILS: LazyLock<Vec<Regex>> = LazyLock::new(|| {
        compile(&[r"(?-u)[A-Za-z0-9._%+-]+@[A-Za-z0-9-]+(?:\.[A-Za-z0-9-]+)*\.[A-Za-z]{2,}"])
    });
    static IP_ADDRESSES: LazyLock<Vec<Regex>> = LazyLock::new(|| {
        compile(&[
            r"(?-u)\b\d{1,3}(?:\.\d{1,3}){3}\b",
            r"(?-u)(?:[0-9A-Fa-f]{0,4}:){2,7}[0-9A-Fa-f]{0,4}",
        ])
    });
    static HOME_DIRECTORIES: LazyLock<Vec<Regex>> =
        LazyLock::new(|| compile(&[r"(?-u)/(?:home|Users)/(?<value>[^/\s\x00]+)"]));
    static SQL_LITERALS: LazyLock<Vec<Regex>> =
        LazyLock::new(|| compile(&[r"(?-u)'(?:[^']|'')*'", r"(?-u)\b\d+(?:\.\d+)?\b"]));

    match builtin {
        Builtin::SecretFlags => &SECRET_FLAGS,
        Builtin::UrlPasswords => &URL_PASSWORDS,
        Builtin::Tokens => &TOKENS,
        Builtin::Cards => &CARDS,
        Builtin::Emails => &EMAILS,
        Builtin::IpAddresses => &IP_ADDRESSES,
        Builtin::HomeDirectories => &HOME_DIRECTORIES,
        Builtin::SqlLiterals => &SQL_LITERALS,
    }
}

fn luhn(found: &[u8]) -> bool {
    let mut count = 0_u32;
    let mut sum = 0_u32;
    for &byte in found.iter().rev() {
        if !byte.is_ascii_digit() {
            continue;
        }
        let digit = u32::from(byte - b'0');
        sum += if count.is_multiple_of(2) {
            digit
        } else {
            let doubled = digit * 2;
            if doubled > 9 { doubled - 9 } else { doubled }
        };
        count += 1;
        if count > 19 {
            return false;
        }
    }
    (13..=19).contains(&count) && sum.is_multiple_of(10)
}

/// Validates IP regex hits and rejects IPv6 matches adjacent to identifier or
/// colon characters, requiring brackets around `::` to avoid C++/Rust scope
/// operators.
fn ip_in_context(value: &[u8], range: &Range<usize>) -> bool {
    let Some(found) = value.get(range.clone()) else {
        return false;
    };
    let Ok(text) = std::str::from_utf8(found) else {
        return false;
    };
    if text.parse::<Ipv4Addr>().is_ok() {
        return true;
    }
    if text.parse::<Ipv6Addr>().is_err() {
        return false;
    }
    let is_ident = |b: u8| b.is_ascii_alphanumeric() || b == b'_' || b == b':';
    if range.start > 0 && value.get(range.start - 1).is_some_and(|&b| is_ident(b)) {
        return false;
    }
    if value.get(range.end).is_some_and(|&b| is_ident(b)) {
        return false;
    }
    if text == "::" {
        let bracketed = range.start > 0
            && value.get(range.start - 1) == Some(&b'[')
            && value.get(range.end) == Some(&b']');
        if !bracketed {
            return false;
        }
    }
    true
}
