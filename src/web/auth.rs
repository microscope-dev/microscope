//! Who may use the server: tokens, the cookie that carries one, and the
//! `Host` and `Origin` checks that keep other web pages out.
//!
//! The socket can do anything a debugger can, so every request needs a
//! token minted for this run. A join link carries one in its fragment,
//! which browsers never send; the page posts it to `/api/login`, which sets
//! an `HttpOnly` cookie, so ordinary links carry no secret. `Host` must name
//! this server, which defeats DNS rebinding, and `Origin` must be its own,
//! so no other page can use the cookie.

use std::fmt::Write as _;
use std::io::Read as _;
use std::net::SocketAddr;

use super::protocol::Role;

/// The tokens minted for one run of the server.
pub struct Tokens {
    control: String,
    view: String,
}

impl Tokens {
    /// Mints a control token and a view token.
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

    /// The role a token grants, comparing in time independent of where a
    /// wrong guess first differs.
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

fn same(a: &str, b: &str) -> bool {
    a.len() == b.len()
        && a.bytes()
            .zip(b.bytes())
            .fold(0, |difference, (a, b)| difference | (a ^ b))
            == 0
}

/// Random bytes from the kernel, as lowercase hexadecimal.
pub fn random_hex(bytes: usize) -> std::io::Result<String> {
    let mut buffer = vec![0; bytes];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut buffer)?;
    Ok(buffer.iter().fold(String::new(), |mut text, byte| {
        let _ = write!(text, "{byte:02x}");
        text
    }))
}

/// The cookie that carries a token. Browsers share cookies between the
/// ports of one host, so the name includes this server's port.
pub fn cookie_name(port: u16) -> String {
    format!("microscope-{port}")
}

/// The `Set-Cookie` value that stores a token for this server, sent only
/// to its own paths, and only over HTTPS when its pages are served so.
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

/// The address a reverse proxy serves the pages at, such as
/// `https://proxy.example/debug/7/`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublicUrl {
    pub https: bool,

    /// The `Host` the proxy sends, as `proxy.example` or `proxy.example:8080`.
    pub host: String,

    /// The path every route lies under, beginning and ending with `/`.
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

    /// The URL itself, with the slash that ends its path.
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

        // The path goes into the page's HTML and the cookie as it is.
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

/// Finds this server's cookie in a `Cookie` header.
pub fn cookie(header: &str, port: u16) -> Option<&str> {
    let name = cookie_name(port);
    header.split(';').find_map(|pair| {
        let (key, value) = pair.trim().split_once('=')?;
        (key == name).then_some(value)
    })
}

/// Which `Host` and `Origin` values belong to this server.
pub struct Origins {
    hosts: Vec<String>,

    /// Pages served from elsewhere that may use the server anyway, such as
    /// a development server proxying to it.
    extra: Vec<String>,

    /// The proxy the pages are served through, whose host and origin are
    /// this server's as well.
    public: Option<PublicUrl>,
}

impl Origins {
    /// The server listening at `address`. A loopback server is also
    /// `localhost`.
    pub fn new(address: SocketAddr, extra_origins: Vec<String>, public: Option<PublicUrl>) -> Self {
        let port = address.port();
        let mut hosts = vec![match address {
            SocketAddr::V4(v4) => format!("{}:{port}", v4.ip()),
            SocketAddr::V6(v6) => format!("[{}]:{port}", v6.ip()),
        }];
        if address.ip().is_loopback() {
            hosts.push(format!("localhost:{port}"));
        }

        Self {
            hosts,
            extra: extra_origins,
            public,
        }
    }

    /// Whether `host` names this server, directly or through its proxy.
    pub fn knows(&self, host: &str) -> bool {
        self.hosts.iter().any(|known| known == host)
            || self
                .public
                .as_ref()
                .is_some_and(|public| public.host == host)
    }

    /// Whether a request with these headers came from this server's own
    /// pages. A request with no `Origin` is refused: browsers send one on
    /// every WebSocket and every POST.
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
                .is_some_and(|rest| rest == host && self.hosts.iter().any(|known| known == rest))
            || self.extra.iter().any(|extra| extra == origin)
    }

    /// The address links use.
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
        let origins = Origins::new(
            "127.0.0.1:7341".parse().expect("address"),
            vec!["http://127.0.0.1:5173".to_owned()],
            None,
        );
        let allows = |host, origin| origins.allows(host, origin);
        assert!(allows(
            Some("127.0.0.1:7341"),
            Some("http://127.0.0.1:7341")
        ));
        assert!(allows(
            Some("localhost:7341"),
            Some("http://localhost:7341")
        ));
        assert!(allows(
            Some("127.0.0.1:7341"),
            Some("http://127.0.0.1:5173")
        ));

        // Another page, a rebound name, a mismatch, or no browser at all.
        assert!(!allows(Some("127.0.0.1:7341"), Some("http://evil.example")));
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

        let public = Origins::new("0.0.0.0:80".parse().expect("address"), Vec::new(), None);
        assert!(!public.allows(Some("localhost:80"), Some("http://localhost:80")));
    }

    #[test]
    fn a_public_url_adds_its_host_and_origin_and_nothing_else() {
        let public: PublicUrl = "https://Proxy.example/debug/7".parse().expect("a URL");
        assert_eq!(public.host, "proxy.example");
        assert_eq!(public.base, "/debug/7/");
        assert_eq!(public.url(), "https://proxy.example/debug/7/");
        let origins = Origins::new(
            "127.0.0.1:7341".parse().expect("address"),
            Vec::new(),
            Some(public.clone()),
        );
        let allows = |host, origin| origins.allows(Some(host), Some(origin));
        assert!(allows("proxy.example", "https://proxy.example"));
        assert!(allows("127.0.0.1:7341", "http://127.0.0.1:7341"));

        // The proxy's host with another scheme, or with the server's origin.
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
