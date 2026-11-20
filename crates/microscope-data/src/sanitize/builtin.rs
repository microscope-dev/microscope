use std::net::{Ipv4Addr, Ipv6Addr};
use std::ops::Range;
use std::str::FromStr;
use std::sync::LazyLock;

use regex::bytes::Regex;

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

    pub(super) fn found(self, value: &[u8], each: &mut impl FnMut(Range<usize>)) {
        let check: fn(&[u8]) -> bool = match self {
            Self::Cards => luhn,
            Self::IpAddresses => ip,
            _ => |_| true,
        };

        for pattern in patterns(self) {
            for captures in pattern.captures_iter(value) {
                let found = span(&captures);
                if value.get(found.clone()).is_some_and(check) {
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
    static TOKENS: LazyLock<Vec<Regex>> = LazyLock::new(|| {
        compile(&[
            r"(?-u)\b(?:AKIA|ASIA)[0-9A-Z]{16}\b",
            r"(?i-u)aws_secret_access_key[\s\x00]*[=:][\s\x00]*(?<value>[A-Za-z0-9/+=]{40})",
            r"(?-u)\b(?:gh[pousr]_[A-Za-z0-9]{36,255}|github_pat_[A-Za-z0-9_]{22,255})\b",
            r"(?-u)\bxox[abposr]-[A-Za-z0-9-]{10,}",
            r"(?-u)\beyJ[A-Za-z0-9_-]{8,}\.eyJ[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}",
            r"(?s-u)-----BEGIN [A-Z ]*PRIVATE KEY-----.*?(?:-----END [A-Z ]*PRIVATE KEY-----|\z)",
            r"(?i-u)\b(?:bearer|basic)[\s\x00]+(?<value>[A-Za-z0-9._~+/=-]{8,})",
        ])
    });
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
    let digits: Vec<u32> = (found.iter())
        .filter(|byte| byte.is_ascii_digit())
        .map(|&byte| u32::from(byte - b'0'))
        .collect();
    if !(13..=19).contains(&digits.len()) {
        return false;
    }

    let sum: u32 = (digits.iter().rev().enumerate())
        .map(|(position, &digit)| match (position % 2, digit * 2) {
            (0, _) => digit,
            (_, doubled) if doubled > 9 => doubled - 9,
            (_, doubled) => doubled,
        })
        .sum();
    sum.is_multiple_of(10)
}

fn ip(found: &[u8]) -> bool {
    std::str::from_utf8(found)
        .is_ok_and(|text| text.parse::<Ipv4Addr>().is_ok() || text.parse::<Ipv6Addr>().is_ok())
}
