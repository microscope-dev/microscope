//! Socket addresses, parsed from the bytes behind a `sockaddr` pointer.

use std::fmt;
use std::net::{Ipv4Addr, Ipv6Addr};

pub const AF_UNSPEC: u16 = 0;

pub const AF_UNIX: u16 = 1;

pub const AF_INET: u16 = 2;

pub const AF_INET6: u16 = 10;

pub const AF_NETLINK: u16 = 16;

/// A parsed socket address.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SocketAddress {
    Inet {
        address: Ipv4Addr,
        port: u16,
    },
    Inet6 {
        address: Ipv6Addr,
        port: u16,
        flow: u32,
        scope: u32,
    },

    /// A path, an abstract name (starting with NUL), or unnamed.
    Unix(Vec<u8>),
    Netlink {
        pid: u32,
        groups: u32,
    },

    /// A family the parser does not know, with its bytes after the family.
    Other {
        family: u16,
        data: Vec<u8>,
    },
    Unspecified,
}

impl SocketAddress {
    /// Parses `bytes`, which hold `length` bytes the call named.
    #[must_use]
    pub fn parse(bytes: &[u8]) -> Option<Self> {
        let family = u16::from_le_bytes(bytes.get(..2)?.try_into().ok()?);
        let rest = &bytes[2..];
        let port = |rest: &[u8]| Some(u16::from_be_bytes(rest.get(..2)?.try_into().ok()?));
        let word = |slice: Option<&[u8]>| Some(u32::from_le_bytes(slice?.try_into().ok()?));

        Some(match family {
            AF_INET => Self::Inet {
                port: port(rest)?,
                address: Ipv4Addr::from(<[u8; 4]>::try_from(rest.get(2..6)?).ok()?),
            },
            AF_INET6 => Self::Inet6 {
                port: port(rest)?,
                flow: u32::from_be_bytes(rest.get(2..6)?.try_into().ok()?),
                address: Ipv6Addr::from(<[u8; 16]>::try_from(rest.get(6..22)?).ok()?),
                scope: word(rest.get(22..26)).unwrap_or(0),
            },
            AF_UNIX => {
                // A path ends at its first NUL; an abstract name keeps its
                // bytes.
                let path = if rest.first() == Some(&0) {
                    rest.to_vec()
                } else {
                    rest.split(|&byte| byte == 0)
                        .next()
                        .unwrap_or_default()
                        .to_vec()
                };

                Self::Unix(path)
            }
            AF_NETLINK => Self::Netlink {
                pid: word(rest.get(2..6))?,
                groups: word(rest.get(6..10))?,
            },
            AF_UNSPEC => Self::Unspecified,
            family => Self::Other {
                family,
                data: rest.to_vec(),
            },
        })
    }

    /// The address as `-yy` prints an endpoint: `10.0.3.9:5432`,
    /// `[::1]:80`, or a Unix path.
    #[must_use]
    pub fn endpoint(&self) -> String {
        match self {
            Self::Inet { address, port } => format!("{address}:{port}"),
            Self::Inet6 { address, port, .. } => format!("[{address}]:{port}"),
            Self::Unix(path) if path.first() == Some(&0) => {
                format!("@{}", String::from_utf8_lossy(&path[1..]))
            }
            Self::Unix(path) => String::from_utf8_lossy(path).into_owned(),
            Self::Netlink { pid, .. } => format!("{pid}"),
            Self::Other { family, .. } => format!("family {family}"),
            Self::Unspecified => "unspecified".to_owned(),
        }
    }
}

impl fmt::Display for SocketAddress {
    /// As strace prints the structure.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Inet { address, port } => write!(
                f,
                "{{sa_family=AF_INET, sin_port=htons({port}), sin_addr=inet_addr(\"{address}\")}}"
            ),
            Self::Inet6 {
                address,
                port,
                flow,
                scope,
            } => write!(
                f,
                "{{sa_family=AF_INET6, sin6_port=htons({port}), sin6_flowinfo=htonl({flow}), \
                 inet_pton(AF_INET6, \"{address}\", &sin6_addr), sin6_scope_id={scope}}}"
            ),
            Self::Unix(path) if path.is_empty() => write!(f, "{{sa_family=AF_UNIX}}"),
            Self::Unix(path) if path.first() == Some(&0) => write!(
                f,
                "{{sa_family=AF_UNIX, sun_path=@{:?}}}",
                String::from_utf8_lossy(&path[1..])
            ),
            Self::Unix(path) => write!(
                f,
                "{{sa_family=AF_UNIX, sun_path={:?}}}",
                String::from_utf8_lossy(path)
            ),
            Self::Netlink { pid, groups } => write!(
                f,
                "{{sa_family=AF_NETLINK, nl_pid={pid}, nl_groups={groups:08x}}}"
            ),
            Self::Other { family, .. } => write!(f, "{{sa_family={family}, sa_data=...}}"),
            Self::Unspecified => write!(f, "{{sa_family=AF_UNSPEC}}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inet_unix_and_inet6_addresses_parse_and_print_as_strace_does() {
        let inet = [2, 0, 0x15, 0x38, 10, 0, 3, 9, 0, 0, 0, 0, 0, 0, 0, 0];
        let parsed = SocketAddress::parse(&inet).expect("inet");
        assert_eq!(
            parsed.to_string(),
            "{sa_family=AF_INET, sin_port=htons(5432), sin_addr=inet_addr(\"10.0.3.9\")}"
        );
        assert_eq!(parsed.endpoint(), "10.0.3.9:5432");
        let mut unix = vec![1, 0];
        unix.extend_from_slice(b"/run/app.sock\0\0\0");
        assert_eq!(
            SocketAddress::parse(&unix).expect("unix").to_string(),
            "{sa_family=AF_UNIX, sun_path=\"/run/app.sock\"}"
        );
        let mut inet6 = vec![10, 0, 0, 80, 0, 0, 0, 0];
        inet6.extend_from_slice(&Ipv6Addr::LOCALHOST.octets());
        inet6.extend_from_slice(&[0, 0, 0, 0]);
        assert_eq!(
            SocketAddress::parse(&inet6).expect("inet6").endpoint(),
            "[::1]:80"
        );
        assert_eq!(SocketAddress::parse(&[2]), None);
    }
}
