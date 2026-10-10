//! Browser management using original server-rendered HTML and native forms.
mod forms;
mod indexer_views;
mod irc_views;
mod live;
mod requester_views;
mod series_views;
mod session;
mod setup_views;
mod views;

use crate::{
    Result,
    crypto::constant_time_eq,
    engine::{Engine, lock},
    integrations,
    net::parse_url,
    torrent::{FilePriority, SelectionUpdate, TransferPolicy},
};
use forms::{Form, decimal};
use session::{Session, Sessions};
use std::{
    collections::BTreeMap,
    io::Write,
    net::TcpStream,
    sync::{Arc, Mutex},
};

pub(crate) struct Web {
    sessions: Mutex<Sessions>,
}

pub(crate) struct BrowserRequest<'a> {
    pub method: &'a str,
    pub target: &'a str,
    pub headers: &'a BTreeMap<String, String>,
    pub body: &'a [u8],
}

pub(crate) struct Response {
    status: u16,
    body: String,
    content_type: &'static str,
    location: Option<String>,
    cookie: Option<String>,
    allow: &'static str,
}

impl Response {
    fn html(status: u16, body: String) -> Self {
        Self {
            status,
            body,
            content_type: "text/html; charset=utf-8",
            location: None,
            cookie: None,
            allow: "GET, POST",
        }
    }

    fn redirect(location: &str) -> Self {
        Self {
            location: Some(location.to_owned()),
            ..Self::html(303, String::new())
        }
    }

    pub(crate) fn write(self, stream: &mut TcpStream) -> Result<()> {
        let reason = match self.status {
            200 => "OK",
            303 => "See Other",
            400 => "Bad Request",
            401 => "Unauthorized",
            403 => "Forbidden",
            404 => "Not Found",
            405 => "Method Not Allowed",
            415 => "Unsupported Media Type",
            503 => "Service Unavailable",
            _ => "Error",
        };
        let mut head = format!(
            "HTTP/1.1 {} {reason}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\nCache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\nReferrer-Policy: same-origin\r\nContent-Security-Policy: default-src 'none'; style-src 'self'; form-action 'self'; frame-ancestors 'none'; base-uri 'none'; object-src 'none'; script-src '{}'; script-src-attr 'none'; connect-src 'self'\r\nX-Frame-Options: DENY\r\n",
            self.status,
            self.content_type,
            self.body.len(),
            live::integrity()
        );
        if let Some(location) = self.location {
            head.push_str(&format!("Location: {location}\r\n"));
        }
        if let Some(cookie) = self.cookie {
            head.push_str(&format!("Set-Cookie: {cookie}\r\n"));
        }
        if self.status == 405 {
            head.push_str(&format!("Allow: {}\r\n", self.allow));
        }
        head.push_str("\r\n");
        stream
            .write_all(head.as_bytes())
            .and_then(|()| stream.write_all(self.body.as_bytes()))
            .map_err(|error| format!("Cannot write the browser response: {error}"))
    }
}

impl Web {
    pub(crate) fn new() -> Self {
        Self {
            sessions: Mutex::new(Sessions::new()),
        }
    }

    pub(crate) fn handle(
        &self,
        engine: &Arc<Engine>,
        token: &str,
        request: BrowserRequest<'_>,
    ) -> Response {
        match self.route(engine, token, request) {
            Ok(response) => response,
            Err(error) => failure(400, &error, None),
        }
    }

    fn route(
        &self,
        engine: &Arc<Engine>,
        token: &str,
        request: BrowserRequest<'_>,
    ) -> Result<Response> {
        let BrowserRequest {
            method,
            target,
            headers,
            body,
        } = request;
        let (path, query) = target.split_once('?').unwrap_or((target, ""));
        if path.starts_with("/ui/live/") {
            return Ok(self.live(engine, method, path, query, headers));
        }
        let authority = authority(headers)?;
        if matches!(path, "/ui/login" | "/" | "/ui/") && !query.is_empty() {
            return Err("This page does not accept query parameters".into());
        }
        if !matches!(method, "GET" | "POST") {
            return Ok(failure(405, "Use a browser link or form", None));
        }
        if method == "GET" && path == "/ui/live.js" && query.is_empty() {
            return Ok(Response {
                content_type: "text/javascript; charset=utf-8",
                ..Response::html(200, live::SCRIPT.to_owned())
            });
        }
        if method == "GET" && path == "/ui/style.css" && query.is_empty() {
            return Ok(Response {
                content_type: "text/css; charset=utf-8",
                ..Response::html(200, include_str!("style.css").to_owned())
            });
        }
        if method == "GET" && matches!(path, "/" | "/ui/") && query.is_empty() {
            return Ok(Response::redirect("/ui"));
        }
        let id = cookie_id(headers)?;
        let session = lock(&self.sessions)?.get(&id, &authority);
        if method == "GET" && path == "/ui/login" && query.is_empty() {
            if session
                .as_ref()
                .is_some_and(|session| session.origin.is_some())
            {
                return Ok(Response::redirect("/ui"));
            }
            let challenge = match session {
                Some(session) => session,
                None => match lock(&self.sessions)?.challenge(&authority) {
                    Ok(session) => session,
                    Err(error) => return Ok(failure(503, &error, None)),
                },
            };
            return Ok(Response {
                cookie: Some(session::cookie(&challenge)),
                ..Response::html(200, views::login(&challenge, None))
            });
        }
        if method == "POST" {
            if !query.is_empty() {
                return Err("Form actions do not accept query parameters".into());
            }
            if headers.get("content-type").is_none_or(|value| {
                !value.split(';').next().is_some_and(|value| {
                    value
                        .trim()
                        .eq_ignore_ascii_case("application/x-www-form-urlencoded")
                })
            }) {
                return Ok(failure(
                    415,
                    "Submit the form using its buttons",
                    session.as_ref(),
                ));
            }
            let origin = match same_origin(headers, &authority) {
                Ok(origin) => origin,
                Err(error) => return Ok(failure(403, &error, session.as_ref())),
            };
            let form = Form::parse(body)?;
            let Some(session) = session else {
                return Ok(failure(403, "Your session expired. Sign in again", None));
            };
            if !constant_time_eq(form.value("csrf")?.as_bytes(), session.csrf.as_bytes()) {
                return Ok(failure(
                    403,
                    "The form expired or is invalid. Reload the page",
                    Some(&session),
                ));
            }
            if path == "/ui/login" {
                form.only(&["csrf", "token"])?;
                let valid_token =
                    constant_time_eq(form.value("token")?.as_bytes(), token.as_bytes());
                return Ok(
                    match lock(&self.sessions)?.login(
                        &session.id,
                        form.value("csrf")?,
                        &origin,
                        valid_token,
                    )? {
                        Some(session) => Response {
                            cookie: Some(session::cookie(&session)),
                            ..Response::redirect("/ui")
                        },
                        None => Response::html(
                            401,
                            views::login(
                                &session,
                                Some(
                                    "The token was not accepted. After five attempts, reload the sign-in page",
                                ),
                            ),
                        ),
                    },
                );
            }
            if session.origin.as_deref() != Some(&origin) {
                return Ok(failure(
                    403,
                    "Sign in on this address before making changes",
                    None,
                ));
            }
            return self.action(engine, path, &form, &session);
        }
        let Some(mut session) = session.filter(|session| session.origin.is_some()) else {
            return Ok(Response::redirect("/ui/login"));
        };
        let query = Form::parse(query.as_bytes())?;
        session.messages = lock(&self.sessions)?.take_messages(&session.id);
        let page = match path {
            "/ui" => {
                query.only(&[])?;
                views::dashboard(engine, &session)?
            }
            "/ui/setup" => {
                query.only(&[])?;
                setup_views::page(engine, &session)?
            }
            "/ui/requesters" => {
                query.only(&[])?;
                requester_views::list(engine, &session)?
            }
            "/ui/irc" => irc_views::list(engine, &session, &query)?,
            "/ui/indexers" => {
                query.only(&[])?;
                indexer_views::list(engine, &session)
            }?,
            "/ui/jobs" => views::jobs(engine, &session, &query)?,
            "/ui/library" => views::library(engine, &session, &query)?,
            "/ui/search" => {
                query.only(&[])?;
                views::search(&session, None, None)?
            }
            "/ui/transfers" => views::transfers(engine, &session, &query)?,
            "/ui/series" => series_views::list(engine, &session, &query)?,
            "/ui/calendar" => series_views::calendar(engine, &session, &query)?,
            _ => {
                if let Some(id) = path
                    .strip_prefix("/ui/irc/")
                    .filter(|id| crate::irc::valid_announcement_id(id))
                {
                    query.only(&[])?;
                    irc_views::detail(engine, &session, id)?
                } else if let Some(id) = path
                    .strip_prefix("/ui/requesters/")
                    .filter(|id| crate::requesters::valid_id(id))
                {
                    requester_views::detail(engine, &session, &query, id)?
                } else if let Some(id) = path
                    .strip_prefix("/ui/jobs/")
                    .filter(|id| forms::valid_id(id, false))
                {
                    let id = id.to_ascii_lowercase();
                    if lock(&engine.store)?.get(&id).is_none() {
                        return Ok(failure(404, "This job no longer exists", Some(&session)));
                    }
                    views::job(engine, &session, &query, &id)?
                } else if let Some(id) = path
                    .strip_prefix("/ui/transfers/")
                    .filter(|id| forms::valid_id(id, true))
                {
                    views::transfer(engine, &session, &query, &id.to_ascii_lowercase())?
                } else if let Some(id) = path
                    .strip_prefix("/ui/series/")
                    .filter(|id| forms::valid_id(id, false))
                {
                    series_views::detail(engine, &session, &query, &id.to_ascii_lowercase())?
                } else {
                    return Ok(failure(404, "This page does not exist", Some(&session)));
                }
            }
        };
        Ok(Response::html(200, page))
    }

    fn live(
        &self,
        engine: &Engine,
        method: &str,
        path: &str,
        query: &str,
        headers: &BTreeMap<String, String>,
    ) -> Response {
        if method != "GET" {
            return live::error(405, "method");
        }
        let kind = match path {
            "/ui/live/jobs" => "jobs",
            "/ui/live/transfers" => "transfers",
            _ => return live::error(404, "not_found"),
        };
        let Ok(authority) = authority(headers) else {
            return live::error(403, "origin");
        };
        let Ok(id) = cookie_id(headers) else {
            return live::error(401, "authentication");
        };
        let Ok(mut sessions) = lock(&self.sessions) else {
            return live::error(503, "unavailable");
        };
        let Some(session) = sessions
            .get(&id, &authority)
            .filter(|session| session.origin.is_some())
        else {
            return live::error(401, "authentication");
        };
        drop(sessions);
        let Ok(origin) = same_origin(headers, &authority) else {
            return live::error(403, "origin");
        };
        if session.origin.as_deref() != Some(&origin) {
            return live::error(403, "origin");
        }
        let Ok(ids) = live::ids(query, kind == "transfers") else {
            return live::error(400, "query");
        };
        live::snapshot(engine, kind, &ids).unwrap_or_else(|_| live::error(503, "unavailable"))
    }

    fn redirect(
        &self,
        session: &Session,
        location: &str,
        messages: Vec<String>,
    ) -> Result<Response> {
        lock(&self.sessions)?.message(&session.id, messages);
        Ok(Response::redirect(location))
    }

    fn action(
        &self,
        engine: &Arc<Engine>,
        path: &str,
        form: &Form,
        session: &Session,
    ) -> Result<Response> {
        match path {
            "/ui/indexers/control" => {
                form.only(&["csrf", "id", "action", "apply", "plan_id"])?;
                let id = form.value("id")?;
                if form.value("apply")? == "yes" {
                    let q = lock(&self.sessions)?.indexer_preview(
                        &session.id,
                        id,
                        form.value("action")?,
                        form.value("plan_id")?,
                    )?;
                    engine.indexer_control(id, &q)?;
                    lock(&self.sessions)?.clear_indexer_preview(&session.id);
                    return self.redirect(
                        session,
                        "/ui/indexers",
                        vec!["Reviewed source control recorded".into()],
                    );
                }
                form.only(&["csrf", "id", "action"])?;
                let mut v = crate::json::Value::object();
                v.insert("action", form.value("action")?);
                let mut q = crate::indexers::ControlRequest::from_json(&v)?;
                let report = engine.indexer_control(id, &q)?;
                q.apply = true;
                q.plan_id = report
                    .get("plan_id")
                    .and_then(crate::json::Value::as_str)
                    .map(str::to_owned);
                lock(&self.sessions)?.save_indexer_preview(&session.id, id, q)?;
                Ok(Response::html(200, indexer_views::review(session, &report)))
            }
            "/ui/irc/control" => {
                let id = form.value("id")?;
                if !crate::irc::valid_announcement_id(id) {
                    return Err("IRC: invalid announcement ID".into());
                }
                if form.value("apply")? == "yes" {
                    form.only(&["csrf", "id", "action", "apply", "plan_id"])?;
                    let query = lock(&self.sessions)?.irc_preview(
                        &session.id,
                        id,
                        form.value("action")?,
                        form.value("plan_id")?,
                    )?;
                    engine.irc_control(id, &query)?;
                    lock(&self.sessions)?.clear_irc_preview(&session.id);
                    return self.redirect(
                        session,
                        &format!("/ui/irc/{id}"),
                        vec!["Announcement review recorded".into()],
                    );
                }
                form.only(&["csrf", "id", "action"])?;
                let mut value = crate::json::Value::object();
                value.insert("action", form.value("action")?);
                let mut query = crate::irc::ControlRequest::from_json(&value)?;
                let report = engine.irc_control(id, &query)?;
                query.apply = true;
                query.plan_id = report
                    .get("plan_id")
                    .and_then(crate::json::Value::as_str)
                    .map(str::to_owned);
                lock(&self.sessions)?.save_irc_preview(&session.id, id, query)?;
                Ok(Response::html(200, irc_views::review(session, id, &report)))
            }
            "/ui/requesters/sync" => {
                form.only(&["csrf"])?;
                engine.sync_requesters()?;
                self.redirect(
                    session,
                    "/ui/requesters",
                    vec![
                        "Plex accounts polled. Each account retains its own result and cursor"
                            .into(),
                    ],
                )
            }
            "/ui/requesters/control" => {
                let id = form.value("account_id")?;
                if !crate::requesters::valid_id(id) {
                    return Err("Invalid requester account".into());
                }
                if form.value("apply")? == "yes" {
                    form.only(&["csrf", "account_id", "action", "plan_id", "apply"])?;
                    let query = lock(&self.sessions)?.requester_preview(
                        &session.id,
                        id,
                        form.value("action")?,
                        form.value("plan_id")?,
                    )?;
                    engine.requester_control(id, &query)?;
                    lock(&self.sessions)?.clear_requester_preview(&session.id);
                    return self.redirect(session,&format!("/ui/requesters/{id}"),vec!["Reviewed requester decision recorded. Compatible demand and imported files are retained".into()]);
                }
                let mut query = requester_views::query(form)?;
                let report = engine.requester_control(id, &query)?;
                query.apply = true;
                query.plan_id = report
                    .get("plan_id")
                    .and_then(crate::json::Value::as_str)
                    .map(str::to_owned);
                lock(&self.sessions)?.save_requester_preview(&session.id, id, query)?;
                Ok(Response::html(
                    200,
                    requester_views::review(session, id, &report),
                ))
            }
            "/ui/jobs/pack-mapping" => {
                form.only(&["csrf", "id", "file_path"])?;
                let ids = form.ids(false)?;
                if ids.len() != 1 {
                    return Err("Choose one pack episode request".into());
                }
                engine.remap_pack(&ids[0], form.value("file_path")?.into())?;
                self.redirect(session, "/ui/jobs", vec!["Pack mapping corrected and request requeued. Existing native transfer controls still apply".into()])
            }
            "/ui/logout" => {
                form.only(&["csrf"])?;
                lock(&self.sessions)?.remove(&session.id);
                Ok(Response {
                    cookie: Some(session::clear_cookie(session.secure)),
                    ..Response::redirect("/ui/login")
                })
            }
            "/ui/search" => {
                let request = form.request()?;
                let report = integrations::search_report(&engine.config, &request)?;
                Ok(Response::html(
                    200,
                    views::search(session, Some(&report), Some(&request))?,
                ))
            }
            "/ui/requests" => {
                let request = form.request()?;
                if request.kind == "series" {
                    let record = engine.track_series(&request, false, false)?;
                    return self.redirect(
                        session,
                        "/ui/series",
                        vec![format!(
                            "Series monitoring recorded; {} aired request(s) submitted",
                            record
                                .get("submitted")
                                .and_then(crate::json::Value::as_u64)
                                .unwrap_or(0)
                        )],
                    );
                }
                let jobs = engine.submit(request)?;
                let mut messages = vec![format!(
                    "{} request(s) recorded. Existing requests are reused",
                    jobs.len()
                )];
                messages.extend(jobs.iter().take(32).map(|job| {
                    format!(
                        "{}: {}",
                        job.id,
                        integrations::report_text(&job.request.title, 256)
                    )
                }));
                self.redirect(session, "/ui/jobs", messages)
            }
            "/ui/sync" => {
                form.only(&["csrf"])?;
                let count = engine.sync()?;
                self.redirect(
                    session,
                    "/ui/jobs",
                    vec![format!(
                        "Watchlist synchronized: {count} request(s) recorded"
                    )],
                )
            }
            "/ui/jobs/action"
            | "/ui/library/action"
            | "/ui/transfers/action"
            | "/ui/series/action" => {
                form.only(&["csrf", "id", "action"])?;
                let action = form.value("action")?;
                let (allowed, location, native) = match path {
                    "/ui/jobs/action" => (&["cancel", "retry"][..], "/ui/jobs", false),
                    "/ui/library/action" => (&["monitor", "unmonitor"][..], "/ui/library", false),
                    "/ui/series/action" => (&["monitor", "unmonitor"][..], "/ui/series", false),
                    _ => (&["pause", "resume"][..], "/ui/transfers", true),
                };
                if !allowed.contains(&action) {
                    return Err("Choose one of the available actions".into());
                }
                // All identifiers and the operation are validated before any side effect.
                let ids = form.ids(native)?;
                let mut results = Vec::with_capacity(ids.len() + 1);
                let mut succeeded = 0;
                for id in ids {
                    let result = match (path, action) {
                        ("/ui/jobs/action", "cancel") => engine.cancel(&id).map(|_| ()),
                        ("/ui/jobs/action", "retry") => engine.retry(&id).map(|_| ()),
                        ("/ui/library/action", _) => {
                            engine.set_monitored(&id, action == "monitor").map(|_| ())
                        }
                        ("/ui/series/action", _) => engine
                            .configure_series(&id, Some(action == "monitor"), None, None)
                            .map(|_| ()),
                        (_, "pause") => engine.pause_transfer(&id).map(|_| ()),
                        _ => engine.resume_transfer(&id).map(|_| ()),
                    };
                    match result {
                        Ok(()) => {
                            succeeded += 1;
                            results.push(format!("{id}: {action} succeeded"));
                        }
                        Err(error) => results
                            .push(format!("{id}: {}", integrations::report_text(&error, 256))),
                    }
                }
                results.insert(0, format!("{succeeded} of {} action(s) succeeded. Each entry was handled independently", results.len()));
                self.redirect(session, location, results)
            }
            "/ui/series/track" => {
                let request = form.series_request()?;
                let specials = browser_bool(form.value("include_specials")?, false)?;
                let future = browser_bool(form.value("future_only")?, false)?;
                let unmonitored = browser_bool(form.value("unmonitored")?, false)?;
                engine.track_series_with_policy(&request, specials, future, !unmonitored)?;
                self.redirect(session, "/ui/series", vec!["Series monitoring recorded. Existing settings are retained when a scope is already tracked".into()])
            }
            "/ui/series/numbering" => {
                form.only(&["csrf", "id", "changes", "action", "plan_id"])?;
                let ids = form.ids(false)?;
                if ids.len() != 1 {
                    return Err("Choose one tracked series".into());
                }
                let mut body = crate::json::Value::object();
                body.insert("changes", crate::json::parse(form.value("changes")?)?);
                let apply = match form.value("action")? {
                    "preview" => false,
                    "apply" => true,
                    _ => return Err("Unknown numbering action".into()),
                };
                body.insert("apply", apply);
                if apply {
                    body.insert("plan_id", form.value("plan_id")?.to_owned());
                }
                let query = crate::series::NumberingRequest::from_json(&body)?;
                let report = engine.series_numbering(&ids[0], &query)?;
                if apply {
                    self.redirect(session,&format!("/ui/series/{}",ids[0]),vec!["Numbering choices saved. Existing requests and library paths retain their original identities".into()])
                } else {
                    Ok(Response::html(
                        200,
                        series_views::numbering(session, &ids[0], &report),
                    ))
                }
            }
            "/ui/series/pack-search" => {
                form.only(&["csrf", "id", "season", "action", "scope_id", "candidate_id"])?;
                let ids = form.ids(false)?;
                if ids.len() != 1 {
                    return Err("Choose one tracked series".into());
                }
                let mut value = crate::json::Value::object();
                let season = form.value("season")?;
                if season.is_empty() {
                    return Err("Choose a catalog season".into());
                }
                value.insert("season", decimal(season, 9999, "pack season")? as u32);
                value.insert(
                    "apply",
                    match form.value("action")? {
                        "preview" => false,
                        "apply" => true,
                        _ => return Err("Unknown pack search action".into()),
                    },
                );
                for key in ["scope_id", "candidate_id"] {
                    if !form.value(key)?.is_empty() {
                        value.insert(key, form.value(key)?.to_owned());
                    }
                }
                let query = crate::pack::AutoPackRequest::from_json(&value)?;
                if query.apply && query.candidate_id.is_none() {
                    return Err(
                        "Preview the pack decision before using its acquisition button".into(),
                    );
                }
                let report = engine.search_packs(&ids[0], &query)?;
                if query.apply {
                    self.redirect(session, "/ui/jobs", vec!["Automatic pack decision recorded. Existing requests and terminal states are retained".into()])
                } else {
                    Ok(Response::html(
                        200,
                        series_views::pack_search(session, &ids[0], &report),
                    ))
                }
            }
            "/ui/library/group" => {
                let ids = form.ids(false)?;
                if ids.len() != 1 {
                    return Err("Choose one current shared library owner".into());
                }
                if form.value("action")? == "apply" {
                    form.only(&["csrf", "id", "action", "plan_id"])?;
                    let query = lock(&self.sessions)?.group_preview(
                        &session.id,
                        &ids[0],
                        form.value("plan_id")?,
                    )?;
                    engine.library_group(&ids[0], &query)?;
                    lock(&self.sessions)?.clear_group_preview(&session.id);
                    return self.redirect(session, "/ui/library", vec!["Whole-group decision recorded. Replacements promote only after every owner is confirmed".into()]);
                }
                form.only(&[
                    "csrf",
                    "id",
                    "action",
                    "operation",
                    "release_title",
                    "source_value",
                    "file_path",
                ])?;
                if form.value("action")? != "preview" {
                    return Err("Unknown shared-group browser action".into());
                }
                let mut body = crate::json::Value::object();
                body.insert("action", form.value("operation")?.to_owned());
                body.insert("release_title", form.value("release_title")?.to_owned());
                if form.value("operation")? == "replace" {
                    body.insert("source_url", form.value("source_value")?.to_owned());
                    body.insert("file_path", form.value("file_path")?.to_owned());
                } else {
                    form.only(&["csrf", "id", "action", "operation", "release_title"])?;
                }
                let mut query = crate::library::GroupRequest::from_json(&body)?;
                let report = engine.library_group(&ids[0], &query)?;
                query.apply = true;
                query.plan_id = Some(
                    report
                        .get("plan_id")
                        .and_then(crate::json::Value::as_str)
                        .ok_or("Missing group review guard")?
                        .into(),
                );
                lock(&self.sessions)?.save_group_preview(&session.id, &ids[0], query)?;
                Ok(Response::html(
                    200,
                    views::library_group(session, &ids[0], &report),
                ))
            }
            "/ui/series/shared-file" => {
                let ids = form.ids(false)?;
                if ids.len() != 1 {
                    return Err("Choose one tracked series".into());
                }
                if form.value("action")? == "apply" {
                    form.only(&["csrf", "id", "action", "plan_id"])?;
                    let query = lock(&self.sessions)?.shared_preview(
                        &session.id,
                        &ids[0],
                        form.value("plan_id")?,
                    )?;
                    engine.shared_file(&ids[0], &query)?;
                    lock(&self.sessions)?.clear_shared_preview(&session.id);
                    return self.redirect(session,"/ui/jobs",vec!["Shared-file ownership recorded. Each episode will confirm the same imported path in Plex".into()]);
                }
                form.only(&[
                    "csrf",
                    "id",
                    "source_value",
                    "file_path",
                    "season",
                    "episodes",
                    "action",
                ])?;
                if form.value("action")? != "preview" {
                    return Err("Unknown shared-file action".into());
                }
                let mut body = crate::json::Value::object();
                body.insert("source_url", form.value("source_value")?.to_owned());
                body.insert("file_path", form.value("file_path")?.to_owned());
                body.insert(
                    "season",
                    decimal(form.value("season")?, 9999, "shared season")? as u32,
                );
                body.insert("episodes", crate::json::parse(form.value("episodes")?)?);
                let mut query = crate::pack::SharedFileRequest::from_json(&body)?;
                let report = engine.shared_file(&ids[0], &query)?;
                query.apply = true;
                query.plan_id = Some(
                    report
                        .get("plan_id")
                        .and_then(crate::json::Value::as_str)
                        .ok_or("Missing shared preview guard")?
                        .into(),
                );
                lock(&self.sessions)?.save_shared_preview(&session.id, &ids[0], query)?;
                Ok(Response::html(
                    200,
                    series_views::shared_file(session, &ids[0], &report),
                ))
            }
            "/ui/series/packs" => {
                form.only(&["csrf", "id", "source_value", "episodes"])?;
                let ids = form.ids(false)?;
                if ids.len() != 1 {
                    return Err("Choose one tracked series".into());
                }
                let mut body = crate::json::Value::object();
                body.insert("source_url", form.value("source_value")?.to_owned());
                body.insert("episodes", crate::json::parse(form.value("episodes")?)?);
                let pack = crate::pack::PackSubmission::from_json(&body)?;
                let result = engine.submit_pack(&ids[0], &pack)?;
                self.redirect(
                    session,
                    "/ui/jobs",
                    vec![format!(
                        "Pack recorded: {} new episode request(s), {} existing request(s) reused",
                        result
                            .get("submitted")
                            .and_then(crate::json::Value::as_u64)
                            .unwrap_or(0),
                        result
                            .get("reused")
                            .and_then(crate::json::Value::as_u64)
                            .unwrap_or(0)
                    )],
                )
            }
            "/ui/series/settings" | "/ui/series/refresh" | "/ui/series/episodes" => {
                let allowed = match path {
                    "/ui/series/settings" => {
                        &["csrf", "id", "enabled", "include_specials", "start_date"][..]
                    }
                    "/ui/series/refresh" => &["csrf", "id"][..],
                    _ => &["csrf", "id", "season", "episode", "enabled"][..],
                };
                form.only(allowed)?;
                let ids = form.ids(false)?;
                if ids.len() != 1 {
                    return Err("Choose one monitored series".into());
                }
                match path {
                    "/ui/series/settings" => {
                        let enabled = browser_bool(form.value("enabled")?, true)?;
                        let specials = browser_bool(form.value("include_specials")?, true)?;
                        let start = form.value("start_date")?;
                        engine.configure_series(
                            &ids[0],
                            Some(enabled),
                            Some(specials),
                            Some((!start.is_empty()).then(|| start.to_owned())),
                        )?;
                    }
                    "/ui/series/refresh" => {
                        engine.refresh_series(&ids[0])?;
                    }
                    _ => {
                        let season = decimal(form.value("season")?, 9999, "Season")? as u32;
                        let episode = decimal(form.value("episode")?, 99999, "Episode")? as u32;
                        let enabled = browser_bool(form.value("enabled")?, true)?;
                        engine.monitor_series_episode(&ids[0], season, episode, enabled)?;
                    }
                }
                self.redirect(
                    session,
                    "/ui/series",
                    vec![
                        "Series operation completed; existing episode jobs and files were retained"
                            .into(),
                    ],
                )
            }
            "/ui/library/baseline" => {
                form.only(&["csrf", "id", "release_title"])?;
                let ids = form.ids(false)?;
                if ids.len() != 1 {
                    return Err("Choose one library entry".into());
                }
                engine.set_baseline(&ids[0], form.value("release_title")?)?;
                self.redirect(
                    session,
                    "/ui/library",
                    vec!["Release baseline saved".into()],
                )
            }
            "/ui/upgrades" => {
                form.only(&["csrf", "action"])?;
                let apply = match form.value("action")? {
                    "preview" => false,
                    "apply" => true,
                    _ => return Err("Choose preview or apply".into()),
                };
                let report = engine.check_upgrades(apply)?;
                Ok(Response::html(200, views::upgrades(session, &report)?))
            }
            "/ui/transfers/priority" => {
                form.only(&["csrf", "id", "priority"])?;
                let id = single_transfer(form)?;
                let priority = form
                    .value("priority")?
                    .parse::<i32>()
                    .ok()
                    .filter(|priority| (-1000..=1000).contains(priority))
                    .ok_or("Priority must be between -1000 and 1000")?;
                engine.set_transfer_priority(&id, priority)?;
                self.redirect(
                    session,
                    "/ui/transfers",
                    vec!["Queue priority saved".into()],
                )
            }
            "/ui/transfers/selection" => {
                form.only(&["csrf", "id", "action", "index"])?;
                let id = single_transfer(form)?;
                let update = match form.value("action")? {
                    "all" => {
                        if !form.value("index")?.is_empty() {
                            return Err("Select all does not accept a file index".into());
                        }
                        SelectionUpdate::All
                    }
                    "include" => {
                        let index = form
                            .value("index")?
                            .parse::<usize>()
                            .map_err(|_| "Invalid selected file index")?;
                        if index >= 100_000 {
                            return Err("Invalid selected file index".into());
                        }
                        SelectionUpdate::Indices(vec![index])
                    }
                    _ => return Err("Unknown file selection action".into()),
                };
                engine.select_transfer_files(&id, &update)?;
                self.redirect(
                    session,
                    &format!("/ui/transfers/{id}"),
                    vec![
                        "File selection expanded. Existing pause and transfer policy still apply"
                            .into(),
                    ],
                )
            }
            "/ui/transfers/files" => {
                form.only(&["csrf", "id", "index", "priority"])?;
                let id = single_transfer(form)?;
                let index = form.value("index")?;
                if index.is_empty() {
                    return Err("Choose a file index".into());
                }
                let index = decimal(index, 100_000, "File index")? as usize;
                let priority = match form.value("priority")? {
                    "low" => FilePriority::Low,
                    "normal" => FilePriority::Normal,
                    "high" => FilePriority::High,
                    _ => return Err("Choose low, normal or high file priority".into()),
                };
                engine.set_file_priority(&id, index, priority)?;
                self.redirect(
                    session,
                    "/ui/transfers",
                    vec!["File priority saved for required pieces".into()],
                )
            }
            "/ui/transfers/policy" => {
                form.only(&[
                    "csrf",
                    "id",
                    "action",
                    "download_limit_bps",
                    "upload_limit_bps",
                    "seed_ratio_milli",
                    "seed_time_secs",
                ])?;
                let id = single_transfer(form)?;
                let action = form.value("action")?;
                let policy = TransferPolicy {
                    download_limit_bps: decimal(
                        form.value("download_limit_bps")?,
                        1_073_741_824,
                        "Download rate",
                    )?,
                    upload_limit_bps: decimal(
                        form.value("upload_limit_bps")?,
                        1_073_741_824,
                        "Upload rate",
                    )?,
                    seed_ratio_milli: optional_limit(form, "seed_ratio_milli", 1_000_000)?
                        .map(|ratio| ratio as u32),
                    seed_time_secs: optional_limit(form, "seed_time_secs", 315_360_000)?,
                };
                policy.validate()?;
                let policy = match action {
                    "save" => Some(policy),
                    "reset" => None,
                    _ => return Err("Choose save or restore defaults".into()),
                };
                engine.set_transfer_policy(&id, policy)?;
                self.redirect(
                    session,
                    "/ui/transfers",
                    vec!["Transfer policy saved".into()],
                )
            }
            _ => Ok(failure(404, "This action does not exist", Some(session))),
        }
    }
}

fn browser_bool(text: &str, required: bool) -> Result<bool> {
    match text {
        "true" => Ok(true),
        "false" => Ok(false),
        "" if !required => Ok(false),
        _ => Err("Choose an available series setting".into()),
    }
}

fn optional_limit(form: &Form, name: &str, maximum: u64) -> Result<Option<u64>> {
    let text = form.value(name)?;
    if text.is_empty() {
        return Ok(None);
    }
    let number = decimal(text, maximum, name)?;
    if number == 0 {
        return Err(format!("{name} must be positive, or leave it empty"));
    }
    Ok(Some(number))
}

fn single_transfer(form: &Form) -> Result<String> {
    let ids = form.ids(true)?;
    if ids.len() != 1 {
        return Err("Choose one transfer".into());
    }
    Ok(ids[0].clone())
}

fn failure(status: u16, message: &str, session: Option<&Session>) -> Response {
    Response::html(status, views::error(session, message))
}

fn authority(headers: &BTreeMap<String, String>) -> Result<String> {
    let host = headers
        .get("host")
        .ok_or("Missing browser address")?
        .to_ascii_lowercase();
    if host.contains(['/', '?', '#']) {
        return Err("Invalid browser address".into());
    }
    parse_url(&format!("http://{host}"))?;
    Ok(host)
}

fn same_origin(headers: &BTreeMap<String, String>, authority: &str) -> Result<String> {
    if headers
        .get("sec-fetch-site")
        .is_some_and(|value| !matches!(value.as_str(), "same-origin" | "none"))
    {
        return Err("Open the form directly in Mynou before submitting it".into());
    }
    let origin = if let Some(origin) = headers.get("origin") {
        let (_, suffix) = origin.split_once("://").ok_or("Invalid browser origin")?;
        if suffix.contains(['/', '?', '#']) {
            return Err("Invalid browser origin".into());
        }
        parse_url(origin)?
    } else {
        let referer = headers
            .get("referer")
            .ok_or("The browser origin or referring address is required for form actions")?;
        // Native forms may omit Origin. Only an unambiguous same-origin URL
        // supplies the fallback; fetch metadata or a rejected Origin cannot.
        if referer.contains(['#', '\\', ',']) || referer.chars().any(char::is_whitespace) {
            return Err("Invalid browser referring address".into());
        }
        parse_url(referer).map_err(|_| "Invalid browser referring address")?
    };
    let expected = parse_url(&format!("{}://{authority}", origin.scheme))?;
    if origin.origin() != expected.origin() {
        return Err("The form must be submitted from the same browser address".into());
    }
    Ok(origin.origin())
}

fn cookie_id(headers: &BTreeMap<String, String>) -> Result<String> {
    let mut found = None;
    if let Some(header) = headers.get("cookie") {
        for pair in header.split(';') {
            let (name, value) = pair
                .trim()
                .split_once('=')
                .ok_or("Invalid browser cookie")?;
            if name == "mynou_session" {
                if found.is_some()
                    || value.len() != 64
                    || !value
                        .bytes()
                        .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
                {
                    return Err(
                        "Invalid browser session. Clear the cookie and sign in again".into(),
                    );
                }
                found = Some(value.to_owned());
            }
        }
    }
    Ok(found.unwrap_or_default())
}
