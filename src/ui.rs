//! Static web interface: plain HTML, CSS and JavaScript embedded in the binary.
//! The assets hold no secrets and no user data.

pub struct Asset {
    pub content_type: &'static str,
    pub cache_control: &'static str,
    pub body: &'static [u8],
}

/// Same-origin only: no inline script or style, no frames, no forms.
pub const CSP: &str = "default-src 'none'; script-src 'self'; style-src 'self'; connect-src 'self'; img-src 'self' data:; font-src 'self'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'";

/// Pages and code are checked again on every load, so an update shows at once.
const REVALIDATE: &str = "no-cache";
/// A font's name carries its version, so the same name never changes content.
const IMMUTABLE: &str = "public, max-age=31536000, immutable";

/// The typefaces, served from the binary: no request leaves the machine.
/// Both are under the SIL Open Font License; the licences sit in `ui/fonts`.
macro_rules! fonts {
    ($($name:literal),* $(,)?) => {
        &[$((concat!("/fonts/", $name, ".woff2"), include_bytes!(concat!("ui/fonts/", $name, ".woff2")))),*]
    };
}

const FONTS: &[(&str, &[u8])] = fonts![
    "geist-1.9.0",
    "geist-mono-1.9.0",
    "bricolage-grotesque-5.3.0-latin",
    "bricolage-grotesque-5.3.0-latin-ext",
];

macro_rules! modules {
    ($($name:literal),* $(,)?) => {
        &[$((concat!("/js/", $name, ".js"), include_str!(concat!("ui/js/", $name, ".js")))),*]
    };
}

/// The native modules `app.js` imports, served by name only.
const MODULES: &[(&str, &str)] = modules![
    "components",
    "dashboard",
    "dom",
    "format",
    "jobs",
    "library",
    "link",
    "motion",
    "palette",
    "quality",
    "releases",
    "requests",
    "router",
    "session",
    "settings",
    "sources",
    "stats",
    "storage",
    "store",
    "theme",
    "transfers",
];

pub fn asset(path: &str) -> Option<Asset> {
    if let Some((_, body)) = MODULES.iter().find(|(name, _)| *name == path) {
        return Some(Asset {
            content_type: "text/javascript; charset=utf-8",
            cache_control: REVALIDATE,
            body: body.as_bytes(),
        });
    }
    if let Some((_, body)) = FONTS.iter().find(|(name, _)| *name == path) {
        return Some(Asset {
            content_type: "font/woff2",
            cache_control: IMMUTABLE,
            body,
        });
    }
    let (content_type, body) = match path {
        "/" | "/index.html" => ("text/html; charset=utf-8", include_str!("ui/index.html")),
        "/app.css" => ("text/css; charset=utf-8", include_str!("ui/app.css")),
        "/app.js" => ("text/javascript; charset=utf-8", include_str!("ui/app.js")),
        "/favicon.svg" => ("image/svg+xml", include_str!("ui/favicon.svg")),
        _ => return None,
    };
    Some(Asset {
        content_type,
        cache_control: REVALIDATE,
        body: body.as_bytes(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(path: &str) -> &'static str {
        std::str::from_utf8(asset(path).unwrap().body).unwrap()
    }

    fn scripts() -> impl Iterator<Item = (&'static str, &'static str)> {
        [("/app.js", text("/app.js"))]
            .into_iter()
            .chain(MODULES.iter().copied())
    }

    #[test]
    fn only_the_listed_assets_exist() {
        for path in ["/", "/index.html", "/app.css", "/app.js", "/favicon.svg"] {
            assert!(asset(path).is_some_and(|a| !a.body.is_empty()), "{path}");
        }
        for (path, body) in MODULES {
            assert!(!body.is_empty(), "{path}");
        }
        for (path, body) in FONTS {
            assert!(body.starts_with(b"wOF2"), "{path}");
        }
        for path in [
            "/api/status",
            "/ws",
            "/app.js?x=1",
            "/ui",
            "//",
            "/../x",
            "/js/",
            "/js/x.js",
            "/js/../app.js",
            "/dom.js",
            "/fonts/",
            "/fonts/x.woff2",
            "/fonts/../app.css",
        ] {
            assert!(asset(path).is_none(), "{path}");
        }
    }

    #[test]
    fn the_page_needs_no_inline_code_or_remote_origin() {
        let page = text("/");
        assert!(!page.contains("onclick") && !page.contains("javascript:"));
        for part in [page, text("/app.css")]
            .into_iter()
            .chain(scripts().map(|(_, body)| body))
        {
            assert!(!part.contains("http://") && !part.contains("https://"));
            assert!(!part.contains("innerHTML") && !part.contains("eval("));
        }
    }

    /// A font the stylesheet names but the server lacks falls back silently.
    #[test]
    fn every_font_is_served_and_named() {
        let css = text("/app.css");
        for (path, _) in FONTS {
            assert!(css.contains(&format!("url(\"{path}\")")), "{path}");
        }
        for url in css.split("url(\"/fonts/").skip(1) {
            let path = format!("/fonts/{}", &url[..url.find('"').unwrap()]);
            assert!(asset(&path).is_some(), "{path}");
        }
    }

    /// A module missing from the list would load as a 404 and stop the page.
    #[test]
    fn every_import_is_served_and_preloaded() {
        let page = text("/");
        for (path, body) in scripts() {
            let dir = &path[..=path.rfind('/').unwrap()];
            for line in body.lines().filter(|line| line.starts_with("import ")) {
                let spec = line
                    .split('\'')
                    .nth(1)
                    .unwrap_or_else(|| panic!("{path}: {line}"));
                let target = format!(
                    "{dir}{}",
                    spec.strip_prefix("./")
                        .unwrap_or_else(|| panic!("{path}: {spec}"))
                );
                assert!(asset(&target).is_some(), "{path} imports {target}");
            }
        }
        for (path, _) in MODULES {
            assert!(
                page.contains(&format!("<link rel=\"modulepreload\" href=\"{path}\">")),
                "{path}"
            );
        }
    }
}
