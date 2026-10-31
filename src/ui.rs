//! Static web interface: plain HTML, CSS and JavaScript embedded in the binary.
//! The assets hold no secrets and no user data.

pub struct Asset {
    pub content_type: &'static str,
    pub body: &'static str,
}

/// Same-origin only: no inline script or style, no frames, no forms.
pub const CSP: &str = "default-src 'none'; script-src 'self'; style-src 'self'; connect-src 'self'; img-src 'self' data:; base-uri 'none'; form-action 'none'; frame-ancestors 'none'";

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
    "releases",
    "requests",
    "router",
    "session",
    "settings",
    "sources",
    "stats",
    "store",
    "theme",
    "transfers",
];

pub fn asset(path: &str) -> Option<Asset> {
    if let Some((_, body)) = MODULES.iter().find(|(name, _)| *name == path) {
        return Some(Asset {
            content_type: "text/javascript; charset=utf-8",
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
    Some(Asset { content_type, body })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scripts() -> impl Iterator<Item = (&'static str, &'static str)> {
        [("/app.js", asset("/app.js").unwrap().body)]
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
        ] {
            assert!(asset(path).is_none(), "{path}");
        }
    }

    #[test]
    fn the_page_needs_no_inline_code_or_remote_origin() {
        let page = asset("/").unwrap().body;
        assert!(!page.contains("onclick") && !page.contains("javascript:"));
        for part in [page, asset("/app.css").unwrap().body]
            .into_iter()
            .chain(scripts().map(|(_, body)| body))
        {
            assert!(!part.contains("http://") && !part.contains("https://"));
            assert!(!part.contains("innerHTML") && !part.contains("eval("));
        }
    }

    /// A module missing from the list would load as a 404 and stop the page.
    #[test]
    fn every_import_is_served_and_preloaded() {
        let page = asset("/").unwrap().body;
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
