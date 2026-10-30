//! Who may use the server: only this run's links, only from its own pages.

use serde_json::json;

use crate::web::Web;

#[tokio::test]
async fn only_a_join_links_token_from_the_servers_own_page_logs_in() {
    let web = Web::start("login", &[]);
    let host = web.address.to_string();
    let origin = web.origin();
    let token = web.control_token.clone();

    let (status, cookie) = web.post_login(&token, Some(&origin), Some(&host));
    assert_eq!(status, 204);
    let cookie = cookie.expect("a cookie");
    assert!(
        cookie.starts_with(&format!("microscope-{}=", web.address.port())),
        "{cookie}"
    );

    // A wrong token, another page, a rebound name, or no browser at all.
    let refused = [
        web.post_login("c-0000", Some(&origin), Some(&host)),
        web.post_login(&token, Some("http://evil.example"), Some(&host)),
        web.post_login(
            &token,
            Some("http://evil.example:80"),
            Some("evil.example:80"),
        ),
        web.post_login(&token, None, Some(&host)),
    ];

    for (status, cookie) in refused {
        assert_eq!(status, 403);
        assert!(cookie.is_none());
    }
}

#[tokio::test]
async fn the_socket_refuses_other_pages_names_and_cookies() {
    let web = Web::start("socket", &[]);
    let cookie = web.cookie(&web.control_token.clone());
    let origin = web.origin();
    let port = web.address.port();
    let attempts = [
        (None, Some(origin.as_str()), None),
        (
            Some(format!("microscope-{port}=c-0000")),
            Some(origin.as_str()),
            None,
        ),
        (Some(cookie.clone()), Some("http://evil.example"), None),
        (Some(cookie.clone()), None, None),
        (
            Some(cookie.clone()),
            Some("http://evil.example:1"),
            Some("evil.example:1"),
        ),
    ];

    for (cookie, origin, host) in attempts {
        let refused = web
            .connect("refused", cookie.as_deref(), origin, host)
            .await;
        assert_eq!(refused.err(), Some(403), "{cookie:?} {origin:?} {host:?}");
    }

    let mut client = web
        .connect("tab", Some(&cookie), Some(&origin), None)
        .await
        .expect("connect");
    let hello = client.next().await;
    assert_eq!(hello["type"], "hello");
    assert_eq!(hello["role"], "control");
    assert_eq!(hello["version"], 2);
}

#[tokio::test]
async fn a_page_from_an_unknown_host_is_refused() {
    let web = Web::start("pages", &[]);
    let page = web.http(&format!(
        "GET /s/abc HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n",
        web.address
    ));
    assert!(page.starts_with("HTTP/1.1 200"), "{page}");
    assert!(page.contains("content-security-policy"), "{page}");

    let rebound = web.http("GET / HTTP/1.1\r\nHost: evil.example\r\nConnection: close\r\n\r\n");
    assert!(rebound.starts_with("HTTP/1.1 421"), "{rebound}");
}

#[tokio::test]
async fn a_viewer_watches_but_cannot_control_or_share_control() {
    let web = Web::start("viewer", &[]);
    let mut owner = web.control("owner").await;
    let link = owner.ok("share", json!({"role": "view", "to": "/"})).await;
    let link = link["url"].as_str().expect("a link");
    assert!(link.contains("/join#v-"), "{link}");

    let mut viewer = web.joining("viewer", link).await;
    let hello = viewer.next().await;
    assert_eq!(hello["role"], "view");
    assert!(
        hello["name"]
            .as_str()
            .expect("a name")
            .starts_with("guest-")
    );

    for (method, params) in [
        ("pause", json!(null)),
        ("processes", json!(null)),
        ("completePath", json!({"text": "/"})),
        ("launch", json!({"program": "/bin/true"})),
        ("share", json!({"role": "control", "to": "/"})),
        ("reloadViews", json!(null)),
    ] {
        let (kind, _) = viewer.request(method, params).await.expect_err(method);
        assert_eq!(kind, "forbidden", "{method}");
    }

    viewer
        .ok("share", json!({"role": "view", "to": "/s/x"}))
        .await;
    viewer.ok("setName", json!({"name": "robin"})).await;

    owner
        .expect("robin in the presence list", |message| {
            message["type"] == "presence"
                && message["people"].as_array().is_some_and(|people| {
                    people
                        .iter()
                        .any(|person| person["name"] == "robin" && person["role"] == "view")
                })
        })
        .await;
}

#[tokio::test]
async fn malformed_requests_are_answered_not_fatal() {
    let web = Web::start("malformed", &[]);
    let mut client = web.control("tab").await;
    client
        .send_raw(r#"{"id": 5, "method": "format the disk"}"#)
        .await;
    let answer = client
        .expect("an answer to 5", |message| message["id"] == 5)
        .await;
    assert_eq!(answer["type"], "error");
    assert_eq!(answer["error"]["kind"], "invalid");

    client.send_raw("not json").await;
    let answer = client
        .expect("an answer to garbage", |message| {
            message["type"] == "error" && message["id"] == 0
        })
        .await;
    assert_eq!(answer["error"]["kind"], "invalid");

    // The connection still works.
    let (kind, _) = client
        .request("continue", json!({}))
        .await
        .expect_err("nothing to continue");
    assert_eq!(kind, "invalid");
}

#[tokio::test]
async fn the_page_can_ask_whether_its_cookie_is_good() {
    let web = Web::start("check", &[]);
    let check = |cookie: &str| {
        let response = web.http(&format!(
            "POST /api/check HTTP/1.1\r\nHost: {}\r\nOrigin: {}\r\nCookie: {cookie}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            web.address,
            web.origin()
        ));
        response.split(' ').nth(1).map(str::to_owned)
    };

    let cookie = web.cookie(&web.control_token.clone());
    assert_eq!(check(&cookie).as_deref(), Some("204"));
    assert_eq!(check("microscope-1=c-0").as_deref(), Some("403"));
}

#[tokio::test]
async fn a_public_url_serves_everything_under_its_path_to_its_proxy_and_nothing_else() {
    let mut web = Web::start("public", &["--public-url", "https://Proxy.example/debug/7"]);
    assert_eq!(web.base, "/debug/7/");
    let get = |path: &str, host: &str| {
        web.http(&format!(
            "GET {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n"
        ))
    };

    let page = get("/debug/7/s/abc/stop/1/t/2/f/0", "proxy.example");
    assert!(page.starts_with("HTTP/1.1 200"), "{page}");

    // The URL itself, with and without its slash, is the page.
    for root in ["/debug/7/", "/debug/7"] {
        let page = get(root, "proxy.example");
        assert!(
            page.starts_with("HTTP/1.1 200") && page.contains("microscope-base"),
            "{root}: {page}"
        );
    }

    // The proxy's own pages may frame it; no other page may.
    assert!(page.contains("frame-ancestors 'self'"), "{page}");
    for outside in ["/s/abc", "/api/check", "/debug/8/s/abc", "/debug/7x/s/abc"] {
        let refused = get(outside, "proxy.example");
        assert!(refused.starts_with("HTTP/1.1 404"), "{outside}: {refused}");
    }

    let unknown = get("/debug/7/s/abc", "evil.example");
    assert!(unknown.starts_with("HTTP/1.1 421"), "{unknown}");

    // The cookie goes back only under the path, and only over HTTPS.
    let token = web.control_token.clone();
    let login = web.http(&format!(
        "POST /debug/7/api/login HTTP/1.1\r\nHost: proxy.example\r\nOrigin: https://proxy.example\r\n\
         Connection: close\r\nContent-Length: {}\r\n\r\n{token}",
        token.len()
    ));
    assert!(login.starts_with("HTTP/1.1 204"), "{login}");
    assert!(
        login.contains("Path=/debug/7/") && login.contains("Secure"),
        "{login}"
    );

    let (refused, _) = web.post_login(&token, Some("http://proxy.example"), Some("proxy.example"));
    assert_eq!(refused, 403);

    let mut client = web.control("tab").await;
    assert_eq!(client.next().await["type"], "hello");

    // Shared links name the proxy.
    let link = client
        .ok("share", json!({"role": "view", "to": "/s/abc"}))
        .await;
    assert!(
        link["url"]
            .as_str()
            .is_some_and(|url| url.starts_with("https://proxy.example/debug/7/join?to=")),
        "{link}"
    );
    drop(client);
    assert!(web.interrupt().success());
}

/// Renderers run in a frame of their own whose origin is opaque, whose
/// policy allows no request, and whose one script is named by a nonce
/// chosen for each response.
#[tokio::test]
async fn the_renderers_frame_is_sandboxed_and_allows_no_request() {
    let web = Web::start("frame", &[]);
    let get = || {
        web.http(&format!(
            "GET /visualizer-frame HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n",
            web.address
        ))
    };

    let frame = get();
    assert!(frame.starts_with("HTTP/1.1 200"), "{frame}");
    let policy = frame
        .lines()
        .find_map(|line| line.strip_prefix("content-security-policy: "))
        .expect("a policy");

    for directive in [
        "sandbox allow-scripts",
        "default-src 'none'",
        "worker-src blob:",
        "frame-ancestors 'self'",
        "'unsafe-eval'",
    ] {
        assert!(policy.contains(directive), "{directive} in {policy}");
    }

    let nonce = policy
        .split("'nonce-")
        .nth(1)
        .and_then(|rest| rest.split('\'').next())
        .expect("a nonce");
    assert!(nonce.len() >= 32, "{nonce}");
    assert!(
        frame.contains(&format!("<script nonce=\"{nonce}\">")),
        "{frame}"
    );
    assert_eq!(frame.matches("<script").count(), 1, "one script");
    assert!(frame.contains("cache-control: no-store"), "{frame}");
    assert!(!get().contains(nonce), "each response has its own nonce");

    let rebound = web
        .http("GET /visualizer-frame HTTP/1.1\r\nHost: evil.example\r\nConnection: close\r\n\r\n");
    assert!(rebound.starts_with("HTTP/1.1 421"), "{rebound}");
}
