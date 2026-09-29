// Serving the theme (docs/spec.md § 4): the archive's theme/ folder, or the built-in default theme
// (ui/index.html) when the config names none, over the `silk` protocol (http://silk.localhost/ on Windows).

use silk_archive::Archive;
use std::sync::Mutex;
use tauri::http::{Response, StatusCode, header::CONTENT_TYPE};

/// The address the window opens; the protocol handler below answers everything under it.
pub const URL: &str = "http://silk.localhost/";

/// The built-in default theme: one self-contained file.
const DEFAULT: &[u8] = include_bytes!("../ui/index.html");

/// Where the theme comes from: its own handle on the archive, so serving never waits for an install
/// holding the other one. None serves the default theme.
pub struct Theme(pub Option<Mutex<Archive>>);

/// The response for a request to `url_path` (the request URI's path, still percent-encoded).
// ponytail: answered on the UI thread and read whole into memory; a large video would stall the window
// while it inflates. Move to register_asynchronous_uri_scheme_protocol and range requests if themes need that.
pub fn respond(theme: &Theme, url_path: &str) -> Response<Vec<u8>> {
    let found = theme_path(url_path).and_then(|path| {
        let bytes = match &theme.0 {
            Some(archive) => archive.lock().unwrap().theme_file(&path).ok().flatten(),
            None => (path == "index.html").then(|| DEFAULT.to_vec()),
        };
        Some((bytes?, content_type(&path)))
    });
    let (status, body, kind) = match found {
        Some((bytes, kind)) => (StatusCode::OK, bytes, kind),
        None => (StatusCode::NOT_FOUND, b"not found".to_vec(), "text/plain"),
    };
    Response::builder().status(status).header(CONTENT_TYPE, kind).body(body).unwrap()
}

/// The theme-relative file a URL path names: percent-decoded, `/` means `index.html`. None for anything
/// that could step outside the theme folder (`..`, `.`, empty segments, backslashes, drive colons).
pub fn theme_path(url_path: &str) -> Option<String> {
    let path = percent_encoding::percent_decode_str(url_path).decode_utf8().ok()?;
    let path = path.strip_prefix('/').unwrap_or(&path);
    let path = if path.is_empty() || path.ends_with('/') { format!("{path}index.html") } else { path.to_string() };
    let safe = path.split('/').all(|seg| !matches!(seg, "" | "." | "..") && !seg.contains(['\\', ':', '\0']));
    safe.then_some(path)
}

pub fn content_type(path: &str) -> &'static str {
    let ext = path.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase()).unwrap_or_default();
    match ext.as_str() {
        "html" | "htm" => "text/html; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "json" => "application/json",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "webp" => "image/webp",
        "gif" => "image/gif",
        "ico" => "image/x-icon",
        "woff2" => "font/woff2",
        "woff" => "font/woff",
        "ttf" => "font/ttf",
        "mp4" => "video/mp4",
        "webm" => "video/webm",
        "txt" => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_resolve_inside_the_theme() {
        assert_eq!(theme_path("/").as_deref(), Some("index.html"));
        assert_eq!(theme_path("").as_deref(), Some("index.html"));
        assert_eq!(theme_path("/index.html").as_deref(), Some("index.html"));
        assert_eq!(theme_path("/img/logo.png").as_deref(), Some("img/logo.png"));
        assert_eq!(theme_path("/pages/").as_deref(), Some("pages/index.html"));
        assert_eq!(theme_path("/fonts/My%20Font.woff2").as_deref(), Some("fonts/My Font.woff2"));
        assert_eq!(theme_path("/caf%C3%A9.png").as_deref(), Some("café.png"));
    }

    #[test]
    fn paths_that_could_escape_are_refused() {
        for p in [
            "/../config.toml",
            "/img/../../config.toml",
            "/%2e%2e/config.toml",
            "/img/%2E%2E/%2e%2e/payload/app.exe",
            "/a/./b.png",
            "//etc/passwd",
            "/img//x.png",
            "/..%5cconfig.toml",
            "/C:/Windows/win.ini",
            "/a%00.png",
            "/%FF.png",
        ] {
            assert_eq!(theme_path(p), None, "{p}");
        }
    }

    #[test]
    fn content_types() {
        assert_eq!(content_type("index.html"), "text/html; charset=utf-8");
        assert_eq!(content_type("a/b/style.CSS"), "text/css; charset=utf-8");
        assert_eq!(content_type("app.js"), "text/javascript; charset=utf-8");
        assert_eq!(content_type("data.json"), "application/json");
        assert_eq!(content_type("logo.svg"), "image/svg+xml");
        assert_eq!(content_type("logo.png"), "image/png");
        assert_eq!(content_type("hero.JPG"), "image/jpeg");
        assert_eq!(content_type("hero.jpeg"), "image/jpeg");
        assert_eq!(content_type("hero.webp"), "image/webp");
        assert_eq!(content_type("font.woff2"), "font/woff2");
        assert_eq!(content_type("README"), "application/octet-stream");
        assert_eq!(content_type("dir.v2/file"), "application/octet-stream");
    }

    #[test]
    fn default_theme_serves_only_its_page() {
        let r = respond(&Theme(None), "/");
        assert_eq!(r.status(), StatusCode::OK);
        assert_eq!(r.headers()[CONTENT_TYPE], "text/html; charset=utf-8");
        assert_eq!(r.body(), DEFAULT);
        assert_eq!(respond(&Theme(None), "/logo.png").status(), StatusCode::NOT_FOUND);
        assert_eq!(respond(&Theme(None), "/../index.html").status(), StatusCode::NOT_FOUND);
    }
}
