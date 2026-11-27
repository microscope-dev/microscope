//! Access tokens, per-port `HttpOnly` cookies, and `Host`/`Origin` validation.
//!
//! Protects local and proxied `microscope web` instances against CSRF and DNS rebinding.

use std::fmt::Write as _;
use std::io::Read as _;
use std::net::SocketAddr;

use super::protocol::Role;

pub struct Tokens {
    control: String,
    view: String,
}

impl Tokens {
    pub fn mint() -> std::io::Result<Self> {
        Ok(Self {
            control: format!("c-{}", random_hex(16)?),
            view: format!("v-{}", random_hex(16)?),
        })
    }

    #[cfg(test)]
    fn fixed(control: &str, view: &str) -> Self {
        Self {
            control: control.to_owned(),
            view: view.to_owned(),
        }
    }

    pub fn token(&self, role: Role) -> &str {
        match role {
            Role::Control => &self.control,
            Role::View => &self.view,
        }
    }

    pub fn role(&self, token: &str) -> Option<Role> {
        if same(token, &self.control) {
            Some(Role::Control)
        } else if same(token, &self.view) {
            Some(Role::View)
        } else {
            None
        }
    }
}

/// Constant-time comparison to avoid leaking token prefixes via timing.
fn same(a: &str, b: &str) -> bool {
    a.len() == b.len()
        && a.bytes()
            .zip(b.bytes())
            .fold(0, |difference, (a, b)| difference | (a ^ b))
            == 0
}

pub fn random_hex(bytes: usize) -> std::io::Result<String> {
    let mut buffer = vec![0; bytes];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut buffer)?;
    Ok(buffer.iter().fold(String::new(), |mut text, byte| {
        let _ = write!(text, "{byte:02x}");
        text
    }))
}

/// Scopes the cookie name by port so concurrent local servers do not overwrite each other.
pub fn cookie_name(port: u16) -> String {
    format!("microscope-{port}")
}

pub fn set_cookie(port: u16, token: &str, public: Option<&PublicUrl>) -> String {
    let (path, secure) = public.map_or(("/", ""), |public| {
        (
            public.base.as_str(),
            if public.https { "; Secure" } else { "" },
        )
    });

    format!(
        "{}={token}; Path={path}; HttpOnly; SameSite=Strict{secure}",
        cookie_name(port)
    )
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublicUrl {
    pub https: bool,

    pub host: String,

    pub base: String,
}

impl PublicUrl {
    pub fn origin(&self) -> String {
        format!(
            "{}://{}",
            if self.https { "https" } else { "http" },
            self.host
        )
    }

    pub fn url(&self) -> String {
        format!("{}{}", self.origin(), self.base)
    }
}

impl std::str::FromStr for PublicUrl {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, String> {
        let (https, rest) = if let Some(rest) = text.strip_prefix("https://") {
            (true, rest)
        } else if let Some(rest) = text.strip_prefix("http://") {
            (false, rest)
        } else {
            return Err(format!("{text:?} is not an http:// or https:// URL"));
        };

        let (host, path) = rest
            .split_once('/')
            .map_or((rest, ""), |(host, path)| (host, path));
        let host_ok = !host.is_empty()
            && host
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b".-:[]".contains(&byte));
        if !host_ok {
            return Err(format!("{text:?} has no host this server could check"));
        }

        let plain = path
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-._~/".contains(&byte));
        if !plain || path.split('/').any(|part| part == ".." || part == ".") {
            return Err(format!(
                "{text:?} has a path this server would not serve under"
            ));
        }

        let trimmed = path.trim_matches('/');
        let base = if trimmed.is_empty() {
            "/".to_owned()
        } else {
            format!("/{trimmed}/")
        };

        Ok(Self {
            https,
            host: host.to_ascii_lowercase(),
            base,
        })
    }
}

pub fn cookie(header: &str, port: u16) -> Option<&str> {
    let name = cookie_name(port);
    header.split(';').find_map(|pair| {
        let (key, value) = pair.trim().split_once('=')?;
        (key == name).then_some(value)
    })
}

/// Allowed `Host` and `Origin` pairs derived from the bind address and `--public-url`.
///
/// Wildcard binds (`0.0.0.0`, `[::]`) allow any IP literal on the listening port plus the
/// machine's hostname, while rejecting arbitrary domains to block DNS rebinding.
pub struct Origins {
    hosts: Vec<String>,

    every_address: Option<u16>,

    public: Option<PublicUrl>,
}

impl Origins {
    pub fn new(address: SocketAddr, hostname: Option<&str>, public: Option<PublicUrl>) -> Self {
        let port = address.port();
        let literal = match address {
            SocketAddr::V4(v4) => format!("{}:{port}", v4.ip()),
            SocketAddr::V6(v6) => format!("[{}]:{port}", v6.ip()),
        };

        let every_address = address.ip().is_unspecified().then_some(port);
        let mut hosts = Vec::new();
        if let Some(hostname) = hostname.filter(|_| every_address.is_some()) {
            hosts.push(format!("{}:{port}", hostname.to_ascii_lowercase()));
        }

        hosts.push(literal);
        if address.ip().is_loopback() {
            hosts.push(format!("localhost:{port}"));
        }

        Self {
            hosts,
            every_address,
            public,
        }
    }

    pub fn knows(&self, host: &str) -> bool {
        self.names_directly(host)
            || self
                .public
                .as_ref()
                .is_some_and(|public| public.host == host)
    }

    fn names_directly(&self, host: &str) -> bool {
        self.hosts.iter().any(|known| known == host)
            || self.every_address.is_some_and(|port| {
                host.parse::<SocketAddr>()
                    .is_ok_and(|address| address.port() == port)
            })
    }

    pub fn allows(&self, host: Option<&str>, origin: Option<&str>) -> bool {
        let Some(host) = host.filter(|host| self.knows(host)) else {
            return false;
        };

        let Some(origin) = origin else {
            return false;
        };

        let proxied = self
            .public
            .as_ref()
            .is_some_and(|public| public.host == host && public.origin() == origin);

        proxied
            || origin
                .strip_prefix("http://")
                .is_some_and(|rest| rest == host && self.names_directly(rest))
    }

    #[expect(
        clippy::indexing_slicing,
        reason = "new always adds the address itself to hosts"
    )]
    pub fn primary(&self) -> &str {
        &self.hosts[0]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_grant_their_role_and_nothing_else() {
        let tokens = Tokens::fixed("c-1234", "v-5678");
        assert_eq!(tokens.role("c-1234"), Some(Role::Control));
        assert_eq!(tokens.role("v-5678"), Some(Role::View));
        for wrong in ["", "c-123", "c-12345", "c-1235", "v-1234", "C-1234"] {
            assert_eq!(tokens.role(wrong), None, "{wrong:?}");
        }

        let minted = Tokens::mint().expect("mint");
        assert_ne!(minted.token(Role::Control), minted.token(Role::View));
        assert_eq!(minted.token(Role::Control).len(), 2 + 32);
    }

    #[test]
    fn the_cookie_is_found_by_this_servers_name() {
        let header = "theme=dark; microscope-9000=v-other; microscope-7341=c-abc ; x=y";
        assert_eq!(cookie(header, 7341), Some("c-abc"));
        assert_eq!(cookie(header, 9000), Some("v-other"));
        assert_eq!(cookie(header, 1), None);
        assert!(set_cookie(7341, "c-abc", None).contains("HttpOnly; SameSite=Strict"));
    }

    #[test]
    fn only_this_servers_own_pages_are_allowed() {
        let origins = Origins::new("127.0.0.1:7341".parse().expect("address"), None, None);
        let allows = |host, origin| origins.allows(host, origin);
        assert!(allows(
            Some("127.0.0.1:7341"),
            Some("http://127.0.0.1:7341")
        ));
        assert!(allows(
            Some("localhost:7341"),
            Some("http://localhost:7341")
        ));

        assert!(!allows(Some("127.0.0.1:7341"), Some("http://evil.example")));
        assert!(!allows(
            Some("127.0.0.1:7341"),
            Some("http://127.0.0.1:5173")
        ));
        assert!(!allows(
            Some("evil.example:7341"),
            Some("http://evil.example:7341")
        ));
        assert!(!allows(
            Some("127.0.0.1:7341"),
            Some("http://localhost:7341")
        ));
        assert!(!allows(
            Some("127.0.0.1:7341"),
            Some("https://127.0.0.1:7341")
        ));
        assert!(!allows(Some("127.0.0.1:7341"), None));
        assert!(!allows(None, Some("http://127.0.0.1:7341")));
        assert_eq!(origins.primary(), "127.0.0.1:7341");

        let public = Origins::new("0.0.0.0:80".parse().expect("address"), None, None);
        assert!(!public.allows(Some("localhost:80"), Some("http://localhost:80")));
    }

    #[test]
    fn every_address_is_any_ip_on_its_port_and_the_hostname() {
        for listen in ["0.0.0.0:7341", "[::]:7341"] {
            let origins = Origins::new(listen.parse().expect("address"), Some("Box1"), None);
            let allows = |host| origins.allows(Some(host), Some(&format!("http://{host}")));
            assert_eq!(origins.primary(), "box1:7341");
            for host in [
                "box1:7341",
                "100.74.16.70:7341",
                "[fd7a::1]:7341",
                "127.0.0.1:7341",
            ] {
                assert!(origins.knows(host), "{listen} {host}");
                assert!(allows(host), "{listen} {host}");
            }

            for host in [
                "100.74.16.70:7342",
                "evil.example:7341",
                "box1",
                "box1.evil.example:7341",
            ] {
                assert!(!origins.knows(host), "{listen} {host}");
            }

            assert!(!origins.allows(Some("10.0.0.1:7341"), Some("http://10.0.0.2:7341")));
        }

        let one = Origins::new(
            "100.74.16.70:7341".parse().expect("address"),
            Some("box1"),
            None,
        );
        assert_eq!(one.primary(), "100.74.16.70:7341");
        assert!(!one.knows("box1:7341"));
        assert!(!one.knows("10.0.0.1:7341"));
    }

    #[test]
    fn a_public_url_adds_its_host_and_origin_and_nothing_else() {
        let public: PublicUrl = "https://Proxy.example/debug/7".parse().expect("a URL");
        assert_eq!(public.host, "proxy.example");
        assert_eq!(public.base, "/debug/7/");
        assert_eq!(public.url(), "https://proxy.example/debug/7/");
        let origins = Origins::new(
            "127.0.0.1:7341".parse().expect("address"),
            None,
            Some(public.clone()),
        );
        let allows = |host, origin| origins.allows(Some(host), Some(origin));
        assert!(allows("proxy.example", "https://proxy.example"));
        assert!(allows("127.0.0.1:7341", "http://127.0.0.1:7341"));

        assert!(!allows("proxy.example", "http://proxy.example"));
        assert!(!allows("proxy.example", "http://127.0.0.1:7341"));
        assert!(!allows("127.0.0.1:7341", "https://proxy.example"));
        assert!(!allows("evil.example", "https://proxy.example"));

        let cookie = set_cookie(7341, "c-1", Some(&public));
        assert!(
            cookie.contains("Path=/debug/7/") && cookie.ends_with("; Secure"),
            "{cookie}"
        );
        assert!(set_cookie(7341, "c-1", None).contains("Path=/;"));

        for bad in [
            "proxy.example/x",
            "ftp://proxy.example/",
            "https:///x",
            "https://p.example/a/../b",
            "https://p.example/a?b",
            "https://us er/",
        ] {
            assert!(bad.parse::<PublicUrl>().is_err(), "{bad}");
        }

        assert_eq!(
            "http://p.example".parse::<PublicUrl>().expect("a URL").base,
            "/"
        );
    }
}
