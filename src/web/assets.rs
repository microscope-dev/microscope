//! The page's files.
//!
//! A release build embeds `build/web`, which `just web` builds. A
//! development build reads the directory at run time instead, so rebuilding
//! the page never rebuilds microscope.

use std::borrow::Cow;

/// The built page, embedded by `build.rs` in release builds.
#[cfg(not(debug_assertions))]
static EMBEDDED: &[(&str, &[u8])] = include!(concat!(env!("OUT_DIR"), "/web_assets.rs"));

/// Where `just web` puts the built page.
#[cfg(debug_assertions)]
const DIRECTORY: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/build/web");

/// Shown when the page has not been built. It has a `<head>` so that
/// `rooted` names the base in it as in the built page.
const UNBUILT: &str = "<!doctype html><head><meta charset=utf-8><title>microscope web</title></head>\
<body style=\"font:15px system-ui;padding:2em\"><h1>The page is not built</h1>\
<p>Run <code>just web</code>, then reload.</p>";

/// A file to serve and its media type.
pub struct Asset {
    pub bytes: Cow<'static, [u8]>,
    pub media_type: &'static str,

    /// Whether the name carries a content hash, so it can be cached forever.
    pub immutable: bool,

    /// Whether it is the page itself, `index.html`.
    pub page: bool,
}

/// Finds the file for a request path. Paths without an extension are the
/// page's own routes, which all load `index.html`.
pub fn find(path: &str) -> Option<Asset> {
    let name = path.trim_start_matches('/');
    if name
        .split('/')
        .any(|part| part == ".." || part.starts_with('.'))
    {
        return None;
    }

    let page = name.is_empty()
        || !name
            .rsplit('/')
            .next()
            .is_some_and(|last| last.contains('.'));
    let name = if page { "index.html" } else { name };
    let bytes = read(name)
        .or_else(|| (page && name == "index.html").then_some(Cow::Borrowed(UNBUILT.as_bytes())))?;

    Some(Asset {
        bytes,
        media_type: media_type(name),
        immutable: name.starts_with("assets/"),
        page: name == "index.html",
    })
}

/// The page as served under `base`: the build names its files relative to
/// the page (`./assets/…`), which a deeper route such as `/s/k7q2/stop/1`
/// would resolve wrongly, so they are rooted at `base`, and the script finds
/// `base` in a `microscope-base` meta tag.
pub fn rooted(page: &[u8], base: &str) -> Vec<u8> {
    let page = String::from_utf8_lossy(page)
        .replace("src=\"./", &format!("src=\"{base}"))
        .replace("href=\"./", &format!("href=\"{base}"));

    page.replacen(
        "<head>",
        &format!("<head>\n    <meta name=\"microscope-base\" content=\"{base}\" />"),
        1,
    )
    .into_bytes()
}

#[cfg(debug_assertions)]
fn read(name: &str) -> Option<Cow<'static, [u8]>> {
    std::fs::read(std::path::Path::new(DIRECTORY).join(name))
        .ok()
        .map(Cow::Owned)
}

#[cfg(not(debug_assertions))]
fn read(name: &str) -> Option<Cow<'static, [u8]>> {
    EMBEDDED
        .iter()
        .find(|(embedded, _)| *embedded == name)
        .map(|(_, bytes)| Cow::Borrowed(*bytes))
}

fn media_type(name: &str) -> &'static str {
    match name.rsplit('.').next() {
        Some("html") => "text/html; charset=utf-8",
        Some("js") => "text/javascript; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("woff2") => "font/woff2",
        Some("json" | "map") => "application/json",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routes_load_the_page_and_files_are_found_by_name() {
        let page = find("/s/k7q2/stop/12").expect("a route is the page");
        assert_eq!(page.media_type, "text/html; charset=utf-8");
        assert!(find("/").is_some());
        for hostile in ["/../Cargo.toml", "/assets/../../etc/passwd", "/.git/config"] {
            assert!(find(hostile).is_none(), "{hostile}");
        }

        assert!(find("/missing.js").is_none());
    }

    #[test]
    fn the_unbuilt_page_names_its_base() {
        let page = rooted(UNBUILT.as_bytes(), "/debug/7/");
        let page = String::from_utf8(page).expect("the page is UTF-8");
        assert!(
            page.contains("<meta name=\"microscope-base\" content=\"/debug/7/\" />"),
            "{page}"
        );
    }
}
