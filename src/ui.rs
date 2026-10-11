//! Static web interface: plain HTML, CSS and JavaScript embedded in the binary.
//! The assets hold no secrets and no user data, so they are served without
//! authentication; every data path stays behind the Bearer token.

pub struct Asset {
    pub content_type: &'static str,
    pub body: &'static str,
}

/// Same-origin only: no inline script or style, no frames, no forms.
pub const CSP: &str = "default-src 'none'; script-src 'self'; style-src 'self'; connect-src 'self'; img-src 'self' data:; base-uri 'none'; form-action 'none'; frame-ancestors 'none'";

pub fn asset(path: &str) -> Option<Asset> {
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

    #[test]
    fn only_the_four_assets_exist() {
        for path in ["/", "/index.html", "/app.css", "/app.js", "/favicon.svg"] {
            assert!(asset(path).is_some_and(|a| !a.body.is_empty()), "{path}");
        }
        for path in ["/api/status", "/ws", "/app.js?x=1", "/ui", "//", "/../x"] {
            assert!(asset(path).is_none(), "{path}");
        }
    }

    #[test]
    fn the_page_needs_no_inline_code_or_remote_origin() {
        let page = asset("/").unwrap().body;
        assert!(!page.contains("onclick") && !page.contains("javascript:"));
        for part in [
            page,
            asset("/app.js").unwrap().body,
            asset("/app.css").unwrap().body,
        ] {
            assert!(!part.contains("http://") && !part.contains("https://"));
            assert!(!part.contains("innerHTML") && !part.contains("eval("));
        }
    }
}
