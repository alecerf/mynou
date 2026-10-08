//! Read-only setup guidance. These checks never authorize acquisition or import.
use super::{
    session::Session,
    views::{array, e, frame},
};
use crate::{
    Result,
    config::{Config, Source},
    engine::Engine,
    indexers::Authentication,
    json::Value,
    net::parse_url,
};
use std::{env, sync::Arc};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    Missing,
    Attention,
    Configured,
    Optional,
}

impl State {
    fn label(self) -> &'static str {
        match self {
            Self::Missing => "Missing",
            Self::Attention => "Needs attention",
            Self::Configured => "Configured",
            Self::Optional => "Optional, disabled",
        }
    }

    fn code(self) -> &'static str {
        match self {
            Self::Missing => "missing",
            Self::Attention => "attention",
            Self::Configured => "configured",
            Self::Optional => "optional",
        }
    }
}

fn credential_value(value: Option<&str>) -> State {
    match value.map(str::trim) {
        None | Some("") => State::Missing,
        Some(value) if value.contains(['\r', '\n', '\0']) => State::Attention,
        Some(_) => State::Configured,
    }
}

fn credential(name: &str) -> State {
    match env::var(name) {
        Ok(value) => credential_value(Some(&value)),
        Err(env::VarError::NotPresent) => State::Missing,
        Err(env::VarError::NotUnicode(_)) => State::Attention,
    }
}

fn together(a: State, b: State) -> State {
    if a == State::Attention || b == State::Attention {
        State::Attention
    } else if a == State::Missing || b == State::Missing {
        State::Missing
    } else {
        State::Configured
    }
}

fn source_state(source: &Source) -> State {
    if parse_url(&source.url).is_err() {
        return State::Attention;
    }
    match &source.options.authentication {
        Authentication::None => State::Configured,
        Authentication::Bearer { token_env } => credential(token_env),
        Authentication::Basic {
            username_env,
            password_env,
        } => together(credential(username_env), credential(password_env)),
        Authentication::Form(form) => {
            if parse_url(&form.login_url).is_err() {
                State::Attention
            } else {
                together(
                    credential(&form.username_env),
                    credential(&form.password_env),
                )
            }
        }
    }
}

fn catalog_state(cfg: &Config) -> State {
    if !cfg.catalog.enabled {
        return State::Optional;
    }
    if parse_url(&cfg.catalog.url).is_err() {
        return State::Attention;
    }
    let token = credential(&cfg.catalog.token_env);
    let key = credential(&cfg.catalog.api_key_env);
    if token == State::Configured || key == State::Configured {
        State::Configured
    } else if token == State::Attention || key == State::Attention {
        State::Attention
    } else {
        State::Missing
    }
}

fn plex_state(cfg: &Config) -> State {
    if !cfg.plex.enabled {
        return State::Optional;
    }
    if parse_url(&cfg.plex.url).is_err()
        || parse_url(&cfg.plex.watchlist_url).is_err()
        || cfg.plex.movies_section.trim().is_empty()
        || cfg.plex.series_section.trim().is_empty()
    {
        return State::Attention;
    }
    match cfg.plex.token_override.as_deref() {
        Some(value) => credential_value(Some(value)),
        None => credential(&cfg.plex.token_env),
    }
}

fn card(
    body: &mut String,
    id: &str,
    title: &str,
    state: State,
    description: &str,
    link: (&str, &str),
) {
    body.push_str(&format!(
        "<article class=panel id=\"{}\" data-setup=\"{}\" data-status=\"{}\"><h2>{}</h2><p><span class=badge>{}</span></p><p>{}</p><a href=\"{}\">{}</a></article>",
        e(id),
        e(id),
        state.code(),
        e(title),
        state.label(),
        e(description),
        e(link.0),
        e(link.1)
    ));
}

pub(super) fn page(engine: &Arc<Engine>, session: &Session) -> Result<String> {
    let cfg = &engine.config;
    let report = engine.indexers()?;
    let rows = array(report.get("sources").unwrap_or(&Value::Null));
    let torrent = cfg.downloads_enabled;
    let usenet = cfg.usenet.downloads.as_ref().is_some_and(|d| d.enabled);
    let usenet_source = cfg.sources.iter().any(|s| s.options.newznab.is_some());
    let mut enabled = 0_usize;
    let mut configured = 0_usize;
    let mut compatible = 0_usize;
    let mut missing = false;
    for (source, row) in cfg.sources.iter().zip(rows) {
        if row.get("enabled").and_then(Value::as_bool) != Some(true) {
            continue;
        }
        enabled += 1;
        let state = source_state(source);
        missing |= state == State::Missing;
        if state == State::Configured {
            configured += 1;
            if source.options.newznab.is_some() {
                compatible += usize::from(usenet);
            } else {
                compatible += usize::from(torrent);
            }
        }
    }
    let sources = if cfg.sources.is_empty() {
        State::Missing
    } else if enabled == 0 {
        State::Attention
    } else if configured > 0 {
        State::Configured
    } else if missing {
        State::Missing
    } else {
        State::Attention
    };
    let downloads = if !torrent && !usenet {
        State::Missing
    } else if configured > 0 && compatible == 0 {
        State::Attention
    } else {
        State::Configured
    };
    let folders = if cfg.store_dir.is_absolute()
        && cfg.movies_root.is_absolute()
        && cfg.series_root.is_absolute()
        && cfg.downloads.data_dir.is_absolute()
    {
        State::Configured
    } else {
        State::Attention
    };
    let mut body = String::from(
        "<p class=lead>Connect your sources, choose a native download route, then preview one request.</p><section class=notice aria-label=\"What these checks mean\"><p>Configured means the loaded settings and required credential presence look usable. Connections, credentials and folder access have not been tested here. Opening this page never contacts a provider, records a request or writes files.</p></section><section class=grid aria-label=\"Setup checklist\">",
    );
    card(
        &mut body,
        "folders",
        "1. Private state and library folders",
        folders,
        "Set library movie and series roots and mount the same media folders in Plex and Mynou. Paths stay private. Use the deployment guide and doctor command to check access; this page does not test writes or permissions.",
        ("#configuration", "Find configuration guidance"),
    );
    card(
        &mut body,
        "sources",
        "2. Search sources",
        sources,
        &format!(
            "{} source(s) configured; {enabled} enabled; {configured} with usable address and required authentication settings. Add a source in indexers, provide any required private credentials, then review a bounded source probe. Optional API keys and upstream login success are not verified here.",
            cfg.sources.len()
        ),
        ("/ui/indexers", "Review sources and diagnostics"),
    );
    card(
        &mut body,
        "downloads",
        "3. Native downloads",
        downloads,
        if !torrent && !usenet {
            "Enable downloads for torrents or usenet.downloads for Newznab sources. Both are currently disabled. No external downloader is required."
        } else if configured > 0 && compatible == 0 {
            "Your configured sources do not match an enabled native download route. Torrent sources need downloads enabled; Newznab sources need usenet.downloads enabled and a configured provider."
        } else if usenet {
            "A native download route is enabled. Review Usenet greeting and authentication before a first request; the existing guarded probe never reads articles. Source and download format limits still apply."
        } else {
            "Native torrent downloads are enabled. Review source diagnostics, then preview a request. A configured route does not verify peer connectivity or guarantee a matching release."
        },
        if usenet || usenet_source {
            ("/ui/usenet", "Review Usenet connection")
        } else {
            ("/ui/transfers", "Open native transfers")
        },
    );
    card(
        &mut body,
        "catalog",
        "4. Catalog metadata",
        catalog_state(cfg),
        "Catalog lookup helps resolve series and metadata. It is optional for explicit-title movie or episode requests. Enable catalog and supply a valid private token or API key to use it; credential presence is not a connection test.",
        ("#configuration", "Configure catalog privately"),
    );
    card(
        &mut body,
        "plex",
        "5. Plex watchlist and library",
        plex_state(cfg),
        "Plex is optional for direct requests, and required for watchlist automation and confirmed Plex availability. Enable plex, set its addresses and library section IDs, and supply a valid private token. Keep library mounts or path mappings consistent. No Plex connection is tested here.",
        ("#configuration", "Configure Plex privately"),
    );
    body.push_str("</section><section class=panel id=configuration><h2>Where to change settings</h2><p>Edit the installation's mynou.json and private .env outside the browser. Keep credentials in the environment, not source control or screenshots. After changing a generated Docker installation, recreate its service so it loads the new configuration and environment, then return here.</p><p>Read the <a href=\"https://github.com/alecerf/mynou/blob/trunk/docs/deployment.md\" target=_blank rel=\"noopener noreferrer\">deployment guide (opens a new tab)</a> for mounts, Plex, catalog and sources. Run <code>mynou doctor --config mynou.json</code> to check your installed configuration. This page does not edit it.</p></section><section class=panel><h2>Try your first request</h2><ol><li>Review source diagnostics and, for Usenet, its explicit connection probe.</li><li>Preview a movie or episode on Search. A preview contacts sources but records no request and downloads no media.</li><li>Choose Record request only when you want acquisition. Follow it in Jobs; Ready confirms the captured import and any required exact Plex path.</li></ol><div class=actions><a class=button href=/ui/search>Preview a request</a><a href=/ui/jobs>Follow requests in Jobs</a></div></section>");
    Ok(frame("Set up Mynou", "/ui/setup", Some(session), &body))
}

#[cfg(test)]
mod tests {
    use super::{State, credential_value, together};

    #[test]
    fn absent_and_blank_credentials_remain_missing() {
        for value in [None, Some(""), Some(" \t\n ")] {
            assert_eq!(credential_value(value), State::Missing);
        }
    }

    #[test]
    fn embedded_header_controls_need_attention() {
        for value in ["fixture\rheader", "fixture\nheader", "fixture\0header"] {
            assert_eq!(credential_value(Some(value)), State::Attention);
        }
    }

    #[test]
    fn trimmed_credential_presence_is_not_authentication() {
        assert_eq!(credential_value(Some(" fixture-value ")), State::Configured);
    }

    #[test]
    fn required_credentials_preserve_missing_and_invalid_states() {
        assert_eq!(together(State::Configured, State::Missing), State::Missing);
        assert_eq!(together(State::Missing, State::Attention), State::Attention);
        assert_eq!(
            together(State::Configured, State::Configured),
            State::Configured
        );
    }
}
