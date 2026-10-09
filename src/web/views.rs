//! Escaped semantic HTML with an optional content-pinned progress enhancement.
use super::{
    forms::{Form, decimal, encode},
    session::Session,
};
use crate::{
    Result,
    engine::{Engine, lock, public_job},
    integrations::report_text,
    json::Value,
    store::Request,
};
use std::sync::Arc;

const PAGE_SIZE: usize = 50;
const JOB_STATES: &[&str] = &[
    "queued",
    "processing",
    "downloading",
    "scanning",
    "staged",
    "ready",
    "failed",
    "cancelled",
];
const TRANSFER_STATES: &[&str] = &[
    "queued",
    "downloading",
    "paused",
    "ready",
    "selected_ready",
    "failed",
    "seed_limited",
];

pub fn login(session: &Session, message: Option<&str>) -> String {
    let mut body = String::from(
        "<p class=lead>Your library, from request to ready.</p><section class=login><h2>Sign in</h2><p>Use the API token from your Mynou configuration.</p>",
    );
    if let Some(message) = message {
        body.push_str(&format!(
            "<p class=notice role=alert>{}</p>",
            display(message)
        ));
    }
    body.push_str(&form("/ui/login", session));
    body.push_str("<label for=token>API token</label><input id=token name=token type=password required maxlength=4096 autocomplete=off autofocus><button type=submit>Sign in</button></form><p class=muted>This browser stays signed in for up to eight hours.</p><a href=/ui/login>Reload sign-in form</a></section>");
    frame("Welcome to Mynou", "", None, &body)
}

pub fn error(session: Option<&Session>, message: &str) -> String {
    let body = format!(
        "<section class=panel><p role=alert>{}</p><p><a href=/ui>Return to overview</a> · <a href=/ui/login>Sign in</a></p></section>",
        display(message)
    );
    frame("Unable to complete this request", "", session, &body)
}

pub fn dashboard(engine: &Arc<Engine>, session: &Session) -> Result<String> {
    let status = engine.status()?;
    let mut body = String::from(
        "<p class=lead>A clear view of what is moving, what needs attention and what is ready to watch.</p><section class=metrics aria-label=\"Library overview\">",
    );
    for (name, key) in [
        ("Requests", "jobs"),
        ("Active", "active"),
        ("Ready", "ready"),
        ("Native transfers", "torrents"),
    ] {
        body.push_str(&format!(
            "<article class=metric><span>{name}</span><strong>{}</strong></article>",
            status
                .get(key)
                .map(|value| display(&raw_scalar(Some(value))))
                .unwrap_or_else(|| "0".into())
        ));
    }
    body.push_str("</section><section class=grid><article class=panel><h2>Find something to watch</h2><p>Preview matching releases and see why each one is accepted or rejected. Then record a request.</p><a class=button href=/ui/search>Search and request</a></article><article class=panel><h2>Keep your library current</h2><p>Review owned imports, monitoring and release baselines. Preview upgrades before applying them.</p><a class=button href=/ui/library>Open library</a></article></section><section class=panel><h2>Plex watchlist</h2>");
    if engine.config.plex.enabled {
        body.push_str("<p>Synchronize watchlist requests now.</p>");
        body.push_str(&form("/ui/sync", session));
        body.push_str("<button type=submit>Sync watchlist</button></form>");
    } else {
        body.push_str("<p>Plex is not connected. Configure the integration to enable watchlist synchronization.</p>");
    }
    body.push_str("</section><section class=panel><h2>Get started or check your setup</h2><p>See what is configured, what needs attention and how to try your first request.</p><a href=/ui/setup>Check setup</a></section>");
    for key in ["last_sync_error", "last_upgrade_error", "last_series_error"] {
        if let Some(text) = status.get(key).and_then(Value::as_str) {
            body.push_str(&format!("<p class=notice role=alert>{}</p>", display(text)));
        }
    }
    Ok(frame("Overview", "/ui", Some(session), &body))
}

pub fn jobs(engine: &Arc<Engine>, session: &Session, query: &Form) -> Result<String> {
    let browse = Browse::new(query, JOB_STATES)?;
    let mut jobs = lock(&engine.store)?.list();
    jobs.sort_by(|a, b| {
        b.created_at
            .cmp(&a.created_at)
            .then_with(|| a.id.cmp(&b.id))
    });
    jobs.retain(|job| browse.matches(&job.request.title, &job.id, &job.state));
    let (page, offset) = page(jobs.len(), browse.page);
    let mut body = String::from(
        "<p class=lead>Follow each request through search, download and library import.</p><a class=button href=/ui/search>New request</a>",
    );
    body.push_str(&live_controls("jobs"));
    body.push_str(&browse.filter("/ui/jobs", JOB_STATES));
    body.push_str(&form("/ui/jobs/action", session));
    body.push_str("<div class=table-wrap><table><caption>Requests</caption><thead><tr><th scope=col>Select</th><th scope=col>Title</th><th scope=col>State</th><th scope=col>Progress</th><th scope=col>Attempts</th></tr></thead><tbody>");
    for job in jobs.iter().skip(offset).take(PAGE_SIZE) {
        body.push_str(&format!("<tr data-live-id=\"{}\"><td>{}</td><td><a href=\"/ui/jobs/{}\">{}</a><small>{} · {}</small></td><td>{}</td><td>{}</td><td data-live-field=attempts>{}</td></tr>",
            e(&job.id), checkbox(&job.id), e(&job.id), display(&job.request.title), display(&job.request.kind), job.request.year,
            live_badge(&job.state), progress(job.progress), job.attempts));
    }
    body.push_str("</tbody></table></div>");
    body.push_str(&bulk_controls(&[
        ("cancel", "Cancel selected"),
        ("retry", "Retry selected"),
    ]));
    body.push_str("</form>");
    body.push_str(&browse.pager("/ui/jobs", jobs.len(), page));
    Ok(frame("Jobs", "/ui/jobs", Some(session), &body))
}

pub fn library(engine: &Arc<Engine>, session: &Session, query: &Form) -> Result<String> {
    let states = &["monitored", "unmonitored", "baseline_required"];
    let browse = Browse::new(query, states)?;
    let value = engine.library()?;
    let entries: Vec<_> = array(&value)
        .iter()
        .filter(|entry| {
            let title = request_title(entry);
            let state = match browse.state.as_str() {
                "baseline_required"
                    if flag(entry, "baseline_required")
                        || flag(entry, "group_baseline_required") =>
                {
                    "baseline_required"
                }
                _ if flag(entry, "monitored") => "monitored",
                _ => "unmonitored",
            };
            browse.matches(title, text(entry, "id"), state)
        })
        .collect();
    let (page, offset) = page(entries.len(), browse.page);
    let mut body = String::from(
        "<p class=lead>Current owned imports stay available while an upgrade is prepared.</p><section class=panel><h2>Library upgrades</h2><p>A preview contacts your sources without changing jobs. Apply performs a fresh check and records eligible upgrades.</p>",
    );
    body.push_str(&upgrade_buttons(session));
    body.push_str("</section>");
    body.push_str(&browse.filter("/ui/library", states));
    body.push_str(&form("/ui/library/action", session));
    body.push_str("<div class=table-wrap><table><caption>Owned library</caption><thead><tr><th scope=col>Select</th><th scope=col>Title</th><th scope=col>Monitoring</th><th scope=col>Baseline</th><th scope=col>Files</th><th scope=col>Upgrade</th></tr></thead><tbody>");
    for entry in entries.iter().skip(offset).take(PAGE_SIZE) {
        let id = text(entry, "id");
        body.push_str(&format!("<tr><td>{}</td><td><a href=\"/ui/jobs/{}\">{}</a></td><td>{}</td><td>{}</td><td>{}</td><td>{}</td></tr>",
            checkbox(id), e(id), display(request_title(entry)), if flag(entry, "monitored") { "On" } else { "Off" },
            if flag(entry, "group_baseline_required") { "Needs group baseline" } else if entry.get("shared_file").is_some_and(|v|v.as_object().is_some()) { "Group baseline recorded" } else if flag(entry, "baseline_required") { "Needs baseline" } else { "Recorded" },
            if flag(entry, "imports_present") { "Available" } else { "Missing" },
            entry.get("pending_upgrade_id").and_then(Value::as_str).map_or_else(|| if entry.get("shared_file").is_some_and(|v|v.as_object().is_some()) {"Open an owner for group actions".into()} else {"—".into()}, |id| format!("<a href=\"/ui/jobs/{}\">View pending upgrade</a>", e(id)))));
    }
    body.push_str("</tbody></table></div>");
    body.push_str(&bulk_controls(&[
        ("monitor", "Monitor selected"),
        ("unmonitor", "Unmonitor selected"),
    ]));
    body.push_str("</form>");
    body.push_str(&browse.pager("/ui/library", entries.len(), page));
    Ok(frame("Library", "/ui/library", Some(session), &body))
}

pub fn search(
    session: &Session,
    report: Option<&Value>,
    request: Option<&Request>,
) -> Result<String> {
    let mut body = String::from(
        "<p class=lead>Find a release, understand the selection and start a request.</p><section class=panel><h2>What would you like to watch?</h2>",
    );
    body.push_str(&form("/ui/requests", session));
    body.push_str("<div class=fields><div><label for=kind>Content type</label><select id=kind name=kind><option value=movie>Movie</option><option value=episode>Episode</option><option value=series>Series</option><option value=file>Local media file</option></select></div><div class=wide><label for=title>Title</label><input id=title name=title required maxlength=4096 autocomplete=off></div><div><label for=year>Year</label><input id=year name=year type=number min=0 max=9999></div><div><label for=season>Season</label><input id=season name=season type=number min=0 max=9999></div><div><label for=episode>Episode</label><input id=episode name=episode type=number min=0 max=99999></div><div><label for=tmdb_id>TMDB ID (optional)</label><input id=tmdb_id name=tmdb_id type=number min=1 max=9007199254740991></div></div><details><summary>Use a specific source</summary><label for=source_kind>Source type</label><select id=source_kind name=source_kind><option value=auto>Automatic search</option><option value=url>Torrent, magnet or HTTP URL</option><option value=file>File on the Mynou server</option></select><label for=source_value>URL or server file path</label><input id=source_value name=source_value maxlength=8192 autocomplete=off><p class=muted>Server paths refer to files Mynou can access. Submitted source URLs are kept private.</p></details><div class=actions><button type=submit formaction=/ui/search class=secondary>Preview search</button><button type=submit>Record request</button></div></form><p class=muted>Preview searches support movies and individual episodes. Series requests create durable monitoring and an episode calendar. Recording a request runs automatic selection again; a preview does not reserve a release.</p></section>");
    if let Some(request) = request {
        body = body.replace(
            "<input id=title name=title required",
            &format!(
                "<input id=title name=title value=\"{}\" required",
                display(&request.title)
            ),
        );
        body = body.replace(
            &format!("<option value={}>", request.kind),
            &format!("<option value={} selected>", request.kind),
        );
        for (name, number) in [
            ("year", u64::from(request.year)),
            ("season", u64::from(request.season)),
            ("episode", u64::from(request.episode)),
            ("tmdb_id", request.tmdb_id.unwrap_or(0)),
        ] {
            if number != 0 {
                body = body.replace(
                    &format!("<input id={name} name={name} type=number"),
                    &format!("<input id={name} name={name} value={number} type=number"),
                );
            }
        }
        if request.source_url.is_some() || request.source_path.is_some() {
            body.push_str("<p class=notice>The source field was cleared after preview. Enter it again before recording this request.</p>");
        }
    }
    if let Some(report) = report {
        body.push_str("<section class=panel><h2>Search preview</h2>");
        if flag(report, "manual_override") {
            body.push_str(
                "<p>A specific source was supplied. Automatic release selection is bypassed.</p>",
            );
        } else {
            body.push_str(&format!(
                "<p>Profile: <strong>{}</strong>. {} candidate(s); {} reported.</p>",
                display(text(report, "profile")),
                scalar(report, "candidate_count"),
                scalar(report, "reported_count")
            ));
            if let Some(indexers) = report.get("indexers") {
                body.push_str(&format!(
                    "<p>{} source(s) responded; {} failed.</p>",
                    scalar(indexers, "successful"),
                    scalar(indexers, "failed")
                ));
            }
            for (key, label) in [
                ("accepted", "Accepted releases"),
                ("rejected", "Rejected releases"),
            ] {
                body.push_str(&format!("<h3>{label}</h3><div class=table-wrap><table><caption>{label}</caption><thead><tr><th scope=col>Release</th><th scope=col>Source</th><th scope=col>Seeds</th><th scope=col>Score</th><th scope=col>Decision</th></tr></thead><tbody>"));
                for candidate in report.get(key).map(array).unwrap_or_default() {
                    let selected = report.get("selected_candidate_id").and_then(Value::as_str)
                        == candidate.get("id").and_then(Value::as_str);
                    let assessment = candidate.get("assessment").unwrap_or(&Value::Null);
                    let reasons = assessment
                        .get("reasons")
                        .map(array)
                        .unwrap_or_default()
                        .iter()
                        .filter_map(Value::as_str)
                        .take(32)
                        .map(display)
                        .collect::<Vec<_>>()
                        .join("; ");
                    body.push_str(&format!(
                        "<tr><td>{}{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td></tr>",
                        display(text(candidate, "title")),
                        if selected {
                            "<small>Automatically selected</small>"
                        } else {
                            ""
                        },
                        display(text(candidate, "source")),
                        scalar(candidate, "seeders"),
                        assessment
                            .get("rank")
                            .map(|rank| scalar(rank, "custom_score"))
                            .unwrap_or_else(|| "0".into()),
                        if reasons.is_empty() {
                            "Matches the configured profile".into()
                        } else {
                            reasons
                        }
                    ));
                }
                body.push_str("</tbody></table></div>");
            }
            if flag(report, "truncated") {
                body.push_str(
                    "<p class=notice>The report is limited. Additional candidates are omitted.</p>",
                );
            }
        }
        body.push_str("<p>Enter the request above to submit it. Source URLs are not included in this preview.</p></section>");
    }
    Ok(frame(
        "Search and request",
        "/ui/search",
        Some(session),
        &body,
    ))
}

pub fn upgrades(session: &Session, report: &Value) -> Result<String> {
    let mut body = format!(
        "<p class=lead>{} entries checked; {} new upgrade(s) queued.</p><p>{}</p>",
        scalar(report, "checked"),
        scalar(report, "queued"),
        if flag(report, "apply") {
            "This check applied eligible upgrades."
        } else {
            "This was a preview. No upgrade jobs were recorded."
        }
    );
    body.push_str(&upgrade_buttons(session));
    body.push_str("<div class=table-wrap><table><caption>Upgrade decisions</caption><thead><tr><th scope=col>Library entry</th><th scope=col>Decision</th><th scope=col>Candidate</th><th scope=col>Upgrade job</th></tr></thead><tbody>");
    for entry in report.get("entries").map(array).unwrap_or_default() {
        body.push_str(&format!(
            "<tr><td><a href=\"/ui/jobs/{}\">{}</a></td><td>{}</td><td>{}</td><td>{}</td></tr>",
            e(text(entry, "job_id")),
            display(text(entry, "job_id")),
            display(text(entry, "action").replace('_', " ").as_str()),
            display(text(entry, "candidate_title")),
            entry
                .get("upgrade_job_id")
                .and_then(Value::as_str)
                .map_or_else(
                    || "—".into(),
                    |id| format!("<a href=\"/ui/jobs/{}\">View upgrade</a>", e(id))
                )
        ));
    }
    body.push_str("</tbody></table></div>");
    if let Some(skipped) = report.get("skipped") {
        body.push_str(&format!(
            "<p>Skipped: {} unmonitored, {} needing a baseline, {} unsupported content type.</p>",
            scalar(skipped, "unmonitored"),
            scalar(skipped, "baseline_required"),
            scalar(skipped, "unsupported_kind")
        ));
    }
    if flag(report, "limited") {
        body.push_str("<p class=notice>The check reached its limit. Additional entries will need another check.</p>");
    }
    body.push_str("<a href=/ui/library>Return to library</a>");
    Ok(frame(
        "Library upgrades",
        "/ui/library",
        Some(session),
        &body,
    ))
}

pub fn job(engine: &Arc<Engine>, session: &Session, query: &Form, id: &str) -> Result<String> {
    query.only(&["files_page", "events_page"])?;
    let files_page = requested_page(query, "files_page")?;
    let events_page = requested_page(query, "events_page")?;
    let (job, events, owned) = {
        let store = lock(&engine.store)?;
        (
            store.get(id).ok_or("Unknown job")?,
            store.events(id),
            store.library_jobs().iter().any(|entry| entry.id == id),
        )
    };
    let value = public_job(&job);
    let mut body = format!(
        "<p class=lead>{}</p>{}<section class=panel><h2>Request details</h2><dl data-live-id=\"{}\"><dt>State</dt><dd>{}</dd><dt>Progress</dt><dd>{}</dd><dt>Content type</dt><dd>{}</dd><dt>Year</dt><dd>{}</dd><dt>Season / episode</dt><dd>{} / {}</dd><dt>Attempts</dt><dd data-live-field=attempts>{}</dd><dt>Source</dt><dd>{}</dd></dl>",
        display(&job.request.title),
        live_controls("jobs"),
        e(id),
        live_badge(&job.state),
        progress(job.progress),
        display(&job.request.kind),
        job.request.year,
        job.request.season,
        job.request.episode,
        job.attempts,
        if job.request.source_path.is_some() {
            "Server file"
        } else if job.request.source_url.is_some() {
            "Private configured source"
        } else {
            "Automatic search"
        }
    );
    if let Some(message) = value.get("last_error").and_then(Value::as_str) {
        body.push_str(&format!(
            "<p class=notice role=alert>{}</p>",
            display(message)
        ));
    }
    if let Some(path) = value.get("pack_file").and_then(Value::as_str) {
        body.push_str(&format!(
            "<p>Mapped torrent file: <code>{}</code></p>",
            display(path)
        ));
        if job.shared_file.is_none()
            && matches!(job.state.as_str(), "failed" | "cancelled")
            && job.imports.is_empty()
        {
            body.push_str(&form("/ui/jobs/pack-mapping", session));
            body.push_str(&hidden("id", id));
            body.push_str("<label for=file_path>Correct mapped torrent path</label><input id=file_path name=file_path required maxlength=4096><button type=submit>Correct mapping and retry</button></form>");
        }
    }
    if let Some(file) = &job.shared_file {
        body.push_str(&format!("<p>Shared ownership: S{:02} E{:02} through E{:02}. All owners retain one library file. Individual remapping and upgrades are blocked.</p>",file.season,file.first_episode,file.last_episode));
    }
    if job.shared_upgrade.is_some() {
        body.push_str("<p>This replacement belongs to a complete shared group. Cancel or retry affects every replacement owner. Staged owners await the remaining Plex confirmations; the previous library group stays current.</p>");
    }
    if let Some(id) = &job.download_id {
        body.push_str(&format!(
            "<p><a href=\"/ui/transfers/{}\">Open native transfer</a></p>",
            e(id)
        ));
    }
    if let Some(id) = &job.upgrade_parent {
        body.push_str(&format!(
            "<p><a href=\"/ui/jobs/{}\">Open previous library entry</a></p>",
            e(id)
        ));
    }
    body.push_str(&form("/ui/jobs/action", session));
    body.push_str(&hidden("id", id));
    body.push_str(&bulk_controls(&[
        ("cancel", "Cancel request"),
        ("retry", "Retry request"),
    ]));
    body.push_str("</form></section>");
    if owned {
        body.push_str("<section class=panel><h2>Library monitoring</h2>");
        body.push_str(&format!(
            "<p>Monitoring is {}.</p>",
            if job.monitored { "on" } else { "off" }
        ));
        body.push_str(&form("/ui/library/action", session));
        body.push_str(&hidden("id", id));
        body.push_str(&bulk_controls(&[
            ("monitor", "Enable monitoring"),
            ("unmonitor", "Disable monitoring"),
        ]));
        body.push_str("</form><h3>Release baseline</h3>");
        if job.shared_file.is_some() {
            body.push_str("<p>Shared ownership uses one baseline and a coordinated replacement for the complete group.</p>");
            body.push_str(&form("/ui/library/group", session));
            body.push_str(&hidden("id", id));
            body.push_str(&hidden("action", "preview"));
            body.push_str(&hidden(
                "operation",
                if job.release.is_some() {
                    "replace"
                } else {
                    "baseline"
                },
            ));
            if let Some(release) = &job.release {
                body.push_str(&format!(
                    "<p>Current baseline: {}</p>",
                    display(&release.title)
                ));
            }
            body.push_str("<label for=group_title>Release title for the complete range or season</label><input id=group_title name=release_title required maxlength=2048>");
            if job.release.is_some() {
                body.push_str("<label for=group_source>Replacement magnet, torrent URL or server torrent path</label><input id=group_source name=source_value required maxlength=8192 autocomplete=off><label for=group_path>Exact replacement video path</label><input id=group_path name=file_path required maxlength=4096><button type=submit>Preview whole-group replacement</button></form>");
            } else {
                body.push_str("<button type=submit>Preview whole-group baseline</button></form>");
            }
        } else if let Some(release) = &job.release {
            body.push_str(&format!("<p>{}</p>", display(&release.title)));
        } else {
            body.push_str(&form("/ui/library/baseline", session));
            body.push_str(&hidden("id", id));
            body.push_str("<label for=release_title>Matching release title</label><input id=release_title name=release_title required maxlength=2048><button type=submit>Save baseline</button></form>");
        }
        body.push_str("</section>");
    }
    let files: Vec<_> = job
        .imports
        .iter()
        .map(|file| ("Library import", file))
        .chain(job.files.iter().map(|file| ("Downloaded source", file)))
        .collect();
    let (file_page, offset) = page(files.len(), files_page);
    body.push_str("<section class=panel><h2>Files</h2><ul class=file-list>");
    for (label, file) in files.iter().skip(offset).take(PAGE_SIZE) {
        body.push_str(&format!(
            "<li><span>{label}</span><code>{}</code></li>",
            display(file)
        ));
    }
    body.push_str("</ul>");
    body.push_str(&detail_pager(
        &format!("/ui/jobs/{id}"),
        "files_page",
        files.len(),
        file_page,
        "events_page",
        events_page,
    ));
    body.push_str("</section><section class=panel><h2>History</h2><div class=table-wrap><table><caption>Recent request events</caption><thead><tr><th scope=col>Time (UTC)</th><th scope=col>State</th><th scope=col>Message</th></tr></thead><tbody>");
    let (event_page, offset) = page(events.len(), events_page);
    for event in events.iter().rev().skip(offset).take(PAGE_SIZE) {
        body.push_str(&format!(
            "<tr><td>{}</td><td>{}</td><td>{}</td></tr>",
            timestamp(event.at),
            badge(&event.state),
            display(&event.message)
        ));
    }
    body.push_str("</tbody></table></div>");
    body.push_str(&detail_pager(
        &format!("/ui/jobs/{id}"),
        "events_page",
        events.len(),
        event_page,
        "files_page",
        file_page,
    ));
    body.push_str("</section>");
    Ok(frame("Job details", "/ui/jobs", Some(session), &body))
}

pub fn library_group(session: &Session, id: &str, report: &Value) -> String {
    let mut body = format!(
        "<p><a href=\"/ui/jobs/{}\">Back to shared library owner</a></p><section class=panel><h2>Review the complete shared group</h2><p>Operation: {}. Owners: {}.</p><p>Release: {}</p><p>Every owner is included. The previous group remains current until all replacement owners confirm the new exact path in Plex. Existing files remain in place.</p>",
        e(id),
        display(text(report, "action")),
        scalar(report, "owners"),
        display(
            report
                .get("release")
                .map_or("", |release| text(release, "title"))
        )
    );
    if let Some(binding) = report.get("binding") {
        body.push_str(&format!("<p>Canonical range: S{} E{} through E{}.</p><p>Torrent: {}</p><p>Exact source path: {}</p><p>Library destination: {}</p>", scalar(binding,"season"), scalar(binding,"first_episode"), scalar(binding,"last_episode"), display(text(binding,"torrent_id")), display(text(binding,"file_path")), display(text(binding,"import_path"))));
    }
    body.push_str(&form("/ui/library/group", session));
    body.push_str(&hidden("id", id));
    body.push_str(&hidden("action", "apply"));
    body.push_str(&hidden("plan_id", text(report, "plan_id")));
    body.push_str(
        "<button type=submit>Record reviewed whole-group decision</button></form></section>",
    );
    frame("Shared-group review", "/ui/library", Some(session), &body)
}

pub fn transfers(engine: &Arc<Engine>, session: &Session, query: &Form) -> Result<String> {
    let browse = Browse::new(query, TRANSFER_STATES)?;
    if !engine.config.downloads_enabled {
        return Ok(frame(
            "Transfers",
            "/ui/transfers",
            Some(session),
            "<section class=panel><p>Native downloads are disabled. Enable them in your configuration to manage transfers here.</p></section>",
        ));
    }
    let value = engine.transfers()?;
    let entries: Vec<_> = array(&value)
        .iter()
        .filter(|entry| {
            browse.matches(
                &transfer_name(entry),
                text(entry, "id"),
                text(entry, "status"),
            )
        })
        .collect();
    let (page, offset) = page(entries.len(), browse.page);
    let mut body = String::from(
        "<p class=lead>Manage the native download queue. Pause, resume and prioritize transfers while retaining files.</p>",
    );
    body.push_str(&live_controls("transfers"));
    body.push_str(&browse.filter("/ui/transfers", TRANSFER_STATES));
    body.push_str(&form("/ui/transfers/action", session));
    body.push_str("<div class=table-wrap><table><caption>Native transfers</caption><thead><tr><th scope=col>Select</th><th scope=col>Transfer</th><th scope=col>State</th><th scope=col>Progress</th><th scope=col>Priority</th><th scope=col>Downloaded / uploaded</th></tr></thead><tbody>");
    for entry in entries.iter().skip(offset).take(PAGE_SIZE) {
        let id = text(entry, "id");
        body.push_str(&format!("<tr data-live-id=\"{}\"><td>{}</td><td><a href=\"/ui/transfers/{}\">{}</a><small><code>{}</code></small></td><td>{}</td><td>{}</td><td>{}</td><td><span data-live-field=downloaded_bytes>{}</span> / <span data-live-field=uploaded_bytes>{}</span> bytes</td></tr>",
            e(id), checkbox(id), e(id), display(&transfer_name(entry)), e(id), live_badge(text(entry, "status")), progress(entry.get("progress").and_then(Value::as_f64).unwrap_or(0.0)),
            scalar(entry, "priority"), scalar(entry, "downloaded_bytes"), scalar(entry, "uploaded_bytes")));
    }
    body.push_str("</tbody></table></div>");
    body.push_str(&bulk_controls(&[
        ("pause", "Pause selected"),
        ("resume", "Resume selected"),
    ]));
    body.push_str("</form>");
    body.push_str(&browse.pager("/ui/transfers", entries.len(), page));
    Ok(frame("Transfers", "/ui/transfers", Some(session), &body))
}

pub fn transfer(engine: &Arc<Engine>, session: &Session, query: &Form, id: &str) -> Result<String> {
    query.only(&["page"])?;
    let requested = requested_page(query, "page")?;
    let value = engine.transfer(id)?;
    let mut body = format!(
        "<p class=lead>{}</p>{}<section class=panel><h2>Transfer details</h2><dl data-live-id=\"{}\"><dt>Identifier</dt><dd><code>{}</code></dd><dt>State</dt><dd>{}</dd><dt>Progress</dt><dd>{}</dd><dt>Downloaded</dt><dd><span data-live-field=downloaded_bytes>{}</span> bytes</dd><dt>Uploaded</dt><dd><span data-live-field=uploaded_bytes>{}</span> bytes</dd><dt>Verified payload</dt><dd><span data-live-field=verified_bytes>{}</span> bytes</dd><dt>Seeding availability</dt><dd><span data-live-field=seed_elapsed_secs>{}</span> seconds</dd></dl><p>{}</p>",
        display(&transfer_name(&value)),
        live_controls("transfers"),
        e(id),
        e(id),
        live_badge(text(&value, "status")),
        progress(value.get("progress").and_then(Value::as_f64).unwrap_or(0.0)),
        scalar(&value, "downloaded_bytes"),
        scalar(&value, "uploaded_bytes"),
        scalar(&value, "verified_bytes"),
        scalar(&value, "seed_elapsed_secs"),
        display(text(&value, "message"))
    );
    body.push_str(&form("/ui/transfers/action", session));
    body.push_str(&hidden("id", id));
    body.push_str(&bulk_controls(&[("pause", "Pause"), ("resume", "Resume")]));
    body.push_str("</form><h3>Related requests</h3><ul>");
    for request in value
        .get("request_ids")
        .map(array)
        .unwrap_or_default()
        .iter()
        .take(32)
        .filter_map(Value::as_str)
    {
        body.push_str(&format!(
            "<li><a href=\"/ui/jobs/{}\">{}</a></li>",
            e(request),
            e(request)
        ));
    }
    body.push_str("</ul></section><section class=grid><article class=panel><h2>Queue priority</h2><p>Higher values run first; ties keep submission order.</p>");
    body.push_str(&form("/ui/transfers/priority", session));
    body.push_str(&hidden("id", id));
    body.push_str(&format!("<label for=priority>Priority</label><input id=priority name=priority type=number required min=-1000 max=1000 value=\"{}\"><button type=submit>Save priority</button></form></article><article class=panel><h2>Transfer policy</h2><p>Rates use bytes per second; zero is unlimited. Empty seeding limits are unlimited. Global bandwidth caps still apply.</p>", scalar(&value, "priority")));
    body.push_str(&form("/ui/transfers/policy", session));
    body.push_str(&hidden("id", id));
    let policy = value
        .get("policy")
        .filter(|value| value.as_object().is_some())
        .or_else(|| value.get("effective_policy"))
        .unwrap_or(&Value::Null);
    for (name, label, minimum, maximum) in [
        ("download_limit_bps", "Download rate", 0, 1_073_741_824_u64),
        ("upload_limit_bps", "Upload rate", 0, 1_073_741_824),
        (
            "seed_ratio_milli",
            "Seed ratio in thousandths (1000 = 1:1)",
            1,
            1_000_000,
        ),
        ("seed_time_secs", "Seeding seconds", 1, 315_360_000),
    ] {
        let current = policy
            .get(name)
            .and_then(Value::as_u64)
            .map_or_else(String::new, |number| number.to_string());
        body.push_str(&format!("<label for={name}>{label}</label><input id={name} name={name} type=number min={minimum} max={maximum} value=\"{current}\">"));
    }
    body.push_str("<div class=actions><button name=action value=save type=submit>Save override</button><button name=action value=reset type=submit class=secondary>Restore defaults</button></div></form><p class=muted>A saved override replaces the seeding defaults in full.</p></article></section>");
    let files = value.get("files").map(array).unwrap_or_default();
    let (page, offset) = page(files.len(), requested);
    body.push_str("<section class=panel><h2>Files and selection</h2><p>Selection retains all files requested by shared jobs. Include more files or download the entire torrent. Priority changes the order of required pieces. Only a complete verified torrent can seed.</p>");
    body.push_str(&form("/ui/transfers/selection", session));
    body.push_str(&hidden("id", id));
    body.push_str("<button name=action value=all type=submit>Download all files</button></form><div class=table-wrap><table><caption>Transfer files</caption><thead><tr><th scope=col>File</th><th scope=col>Bytes</th><th scope=col>Selection / verification</th><th scope=col>Priority</th></tr></thead><tbody>");
    for file in files.iter().skip(offset).take(PAGE_SIZE) {
        body.push_str(&format!(
            "<tr><td><code>{}</code></td><td>{}</td><td>",
            display(text(file, "path")),
            scalar(file, "size_bytes")
        ));
        let selected = file.get("selected").and_then(Value::as_bool) == Some(true);
        let verified = file.get("verified").and_then(Value::as_bool) == Some(true);
        body.push_str(if verified {
            "Verified"
        } else if selected {
            "Required"
        } else {
            "Not selected"
        });
        if !selected {
            body.push_str(&form("/ui/transfers/selection", session));
            body.push_str(&hidden("id", id));
            body.push_str(&hidden("index", &raw_scalar(file.get("index"))));
            body.push_str(
                "<button name=action value=include type=submit>Include file</button></form>",
            );
        }
        body.push_str("</td><td>");
        body.push_str(&form("/ui/transfers/files", session));
        body.push_str(&hidden("id", id));
        body.push_str(&hidden("index", &raw_scalar(file.get("index"))));
        let index = raw_scalar(file.get("index"));
        body.push_str(&format!("<label class=sr-only for=\"file-{index}\">Priority for file {index}</label><select id=\"file-{index}\" name=priority>"));
        for priority in ["low", "normal", "high"] {
            body.push_str(&format!(
                "<option value={priority}{}>{priority}</option>",
                if text(file, "priority") == priority {
                    " selected"
                } else {
                    ""
                }
            ));
        }
        body.push_str("</select><button type=submit>Save</button></form></td></tr>");
    }
    body.push_str("</tbody></table></div>");
    body.push_str(&detail_pager(
        &format!("/ui/transfers/{id}"),
        "page",
        files.len(),
        page,
        "",
        1,
    ));
    body.push_str("</section>");
    Ok(frame(
        "Transfer details",
        "/ui/transfers",
        Some(session),
        &body,
    ))
}

struct Browse {
    q: String,
    state: String,
    page: usize,
}

impl Browse {
    fn new(form: &Form, states: &[&str]) -> Result<Self> {
        form.only(&["q", "state", "page"])?;
        let q = form.value("q")?.trim().to_owned();
        if q.len() > 128 || report_text(&q, 128) != q {
            return Err("Filter using a title or identifier of at most 128 bytes".into());
        }
        let state = form.value("state")?.to_owned();
        if !state.is_empty() && !states.contains(&state.as_str()) {
            return Err("Choose an available state filter".into());
        }
        Ok(Self {
            q,
            state,
            page: requested_page(form, "page")?,
        })
    }

    fn matches(&self, title: &str, id: &str, state: &str) -> bool {
        (self.state.is_empty() || self.state == state)
            && (self.q.is_empty()
                || title.to_lowercase().contains(&self.q.to_lowercase())
                || id.contains(&self.q.to_ascii_lowercase()))
    }

    fn filter(&self, path: &str, states: &[&str]) -> String {
        let mut html = format!(
            "<form method=get action={path} class=filters><div><label for=q>Title or identifier</label><input id=q name=q maxlength=128 value=\"{}\"></div><div><label for=state>State</label><select id=state name=state><option value=\"\">All</option>",
            e(&self.q)
        );
        for state in states {
            html.push_str(&format!(
                "<option value={state}{}>{}</option>",
                if self.state == *state {
                    " selected"
                } else {
                    ""
                },
                e(&state.replace('_', " "))
            ));
        }
        html.push_str("</select></div><button type=submit>Filter</button></form>");
        html
    }

    fn pager(&self, path: &str, count: usize, current: usize) -> String {
        pager(count, current, |page| {
            format!(
                "{path}?q={}&amp;state={}&amp;page={page}",
                encode(&self.q),
                encode(&self.state)
            )
        })
    }
}

pub(super) fn detail_pager(
    path: &str,
    key: &str,
    count: usize,
    current: usize,
    other: &str,
    other_page: usize,
) -> String {
    pager(count, current, |page| {
        if other.is_empty() {
            format!("{path}?{key}={page}")
        } else {
            format!("{path}?{key}={page}&amp;{other}={other_page}")
        }
    })
}

fn pager(count: usize, current: usize, link: impl Fn(usize) -> String) -> String {
    let pages = count.div_ceil(PAGE_SIZE).max(1);
    let mut html = format!(
        "<nav class=pager aria-label=Pagination><span>{count} entries · Page {current} of {pages}</span>"
    );
    if current > 1 {
        html.push_str(&format!(
            "<a href=\"{}\" rel=prev>Previous</a>",
            link(current - 1)
        ));
    }
    if current < pages {
        html.push_str(&format!(
            "<a href=\"{}\" rel=next>Next</a>",
            link(current + 1)
        ));
    }
    html.push_str("</nav>");
    html
}

fn page(count: usize, requested: usize) -> (usize, usize) {
    let page = requested.min(count.div_ceil(PAGE_SIZE).max(1));
    (page, (page - 1) * PAGE_SIZE)
}

fn requested_page(form: &Form, key: &str) -> Result<usize> {
    let value = form.value(key)?;
    if value.is_empty() {
        return Ok(1);
    }
    let page = decimal(value, 100_000, "Page")? as usize;
    if page == 0 {
        return Err("Page numbering starts at one".into());
    }
    Ok(page)
}

pub(super) fn frame(title: &str, active: &str, session: Option<&Session>, body: &str) -> String {
    let mut html = format!(
        "<!doctype html><html lang=en><head><meta charset=utf-8><meta name=viewport content=\"width=device-width, initial-scale=1\"><title>{} · Mynou</title><link rel=stylesheet href=/ui/style.css></head><body><a class=skip href=#main>Skip to content</a><header><a class=brand href=/ui><span class=mark aria-hidden=true>m</span>Mynou</a>",
        e(title)
    );
    if let Some(session) = session {
        html.push_str("<nav aria-label=Main>");
        for (path, label) in [
            ("/ui", "Overview"),
            ("/ui/setup", "Setup"),
            ("/ui/jobs", "Jobs"),
            ("/ui/library", "Library"),
            ("/ui/series", "Series"),
            ("/ui/calendar", "Calendar"),
            ("/ui/search", "Search"),
            ("/ui/transfers", "Transfers"),
            ("/ui/requesters", "Requesters"),
            ("/ui/irc", "Announcements"),
            ("/ui/notifications", "Notifications"),
            ("/ui/indexers", "Indexers"),
        ] {
            html.push_str(&format!(
                "<a href={path}{}>{label}</a>",
                if active == path {
                    " aria-current=page"
                } else {
                    ""
                }
            ));
        }
        html.push_str("</nav>");
        html.push_str(&form("/ui/logout", session));
        html.push_str("<button class=quiet type=submit>Sign out</button></form>");
    }
    html.push_str(&format!("</header><main id=main><h1>{}</h1>", e(title)));
    if let Some(session) = session
        && !session.messages.is_empty()
    {
        html.push_str("<section class=notice role=status aria-label=\"Action results\"><ul>");
        for message in &session.messages {
            html.push_str(&format!("<li>{}</li>", display(message)));
        }
        html.push_str("</ul></section>");
    }
    html.push_str(body);
    if matches!(active, "/ui/jobs" | "/ui/transfers") && body.contains("data-live=") {
        html.push_str(&format!(
            "<script defer src=/ui/live.js integrity=\"{}\"></script>",
            super::live::integrity()
        ));
    }
    html.push_str(&format!(
        "</main><footer>Mynou {} · <a href=/ui>Refresh overview</a></footer></body></html>",
        env!("CARGO_PKG_VERSION")
    ));
    html
}

pub(super) fn form(action: &str, session: &Session) -> String {
    format!(
        "<form method=post action=\"{}\">{}",
        e(action),
        hidden("csrf", &session.csrf)
    )
}
pub(super) fn hidden(name: &str, value: &str) -> String {
    format!(
        "<input type=hidden name=\"{}\" value=\"{}\">",
        e(name),
        e(value)
    )
}
pub(super) fn checkbox(id: &str) -> String {
    format!(
        "<input type=checkbox name=id value=\"{}\" aria-label=\"Select entry {}\">",
        e(id),
        e(id)
    )
}
pub(super) fn bulk_controls(actions: &[(&str, &str)]) -> String {
    let mut html = String::from("<div class=actions>");
    for (value, label) in actions {
        html.push_str(&format!(
            "<button type=submit name=action value={value} class=secondary>{label}</button>"
        ));
    }
    html.push_str("</div><p class=muted>Select up to 32 entries. Results are reported separately; changes retain library and download files.</p>");
    html
}
fn upgrade_buttons(session: &Session) -> String {
    format!(
        "{}<div class=actions><button type=submit name=action value=preview class=secondary>Preview upgrades</button><button type=submit name=action value=apply>Apply available upgrades</button></div></form>",
        form("/ui/upgrades", session)
    )
}
fn live_controls(kind: &str) -> String {
    format!(
        "<section class=\"panel live-controls\" data-live={kind} aria-label=\"Live updates\"><div class=actions><button type=button data-live-toggle disabled>Enable live updates</button><a class=button href=\"\" data-live-refresh>Refresh page</a><a href=/ui/login data-live-sign-in hidden>Sign in again</a></div><p data-live-status role=status aria-live=polite>Live updates paused. JavaScript is optional; use Refresh page.</p><p class=muted data-live-freshness>Showing the page as loaded.</p><p class=muted>Updates change state and progress only. Refresh page for new or removed rows, filter changes, messages or controls.</p><noscript><p>Use Refresh page to see current progress.</p></noscript></section>"
    )
}
fn live_badge(state: &str) -> String {
    format!(
        "<span class=badge data-live-field=state>{}</span>",
        display(&state.replace('_', " "))
    )
}
fn progress(number: f64) -> String {
    let number = if number.is_finite() {
        number.clamp(0.0, 1.0) * 100.0
    } else {
        0.0
    };
    format!(
        "<span data-live-field=progress><progress max=100 value={number:.1} aria-label=\"{number:.1}% complete\">{number:.1}%</progress><small>{number:.1}%</small></span>"
    )
}
pub(super) fn badge(state: &str) -> String {
    format!(
        "<span class=badge>{}</span>",
        display(&state.replace('_', " "))
    )
}
fn request_title(entry: &Value) -> &str {
    entry
        .get("request")
        .map(|request| text(request, "title"))
        .unwrap_or("")
}
fn transfer_name(value: &Value) -> String {
    value
        .get("files")
        .map(array)
        .and_then(|files| files.first())
        .map(|file| text(file, "path").to_owned())
        .unwrap_or_else(|| "Awaiting metadata".into())
}
pub(super) fn array(value: &Value) -> &[Value] {
    value.as_array().unwrap_or_default()
}
pub(super) fn text<'a>(value: &'a Value, key: &str) -> &'a str {
    value.get(key).and_then(Value::as_str).unwrap_or("")
}
pub(super) fn flag(value: &Value, key: &str) -> bool {
    value.get(key).and_then(Value::as_bool).unwrap_or(false)
}
pub(super) fn scalar(value: &Value, key: &str) -> String {
    display(&raw_scalar(value.get(key)))
}
fn raw_scalar(value: Option<&Value>) -> String {
    match value {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Number(number)) => number.to_string(),
        Some(Value::Bool(flag)) => flag.to_string(),
        _ => String::new(),
    }
}
pub(super) fn display(text: &str) -> String {
    e(&report_text(text, 2048))
}
pub(super) fn e(text: &str) -> String {
    let mut result = String::with_capacity(text.len());
    for character in text.chars() {
        match character {
            '&' => result.push_str("&amp;"),
            '<' => result.push_str("&lt;"),
            '>' => result.push_str("&gt;"),
            '"' => result.push_str("&quot;"),
            '\'' => result.push_str("&#39;"),
            character => result.push(character),
        }
    }
    result
}

fn timestamp(seconds: u64) -> String {
    // Gregorian civil date from a bounded Unix timestamp; integer arithmetic only.
    if seconds > 253_402_300_799 {
        return seconds.to_string();
    }
    let days = seconds / 86_400;
    let z = days as i64 + 719_468;
    let era = z / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let mut year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = mp + if mp < 10 { 3 } else { -9 };
    year += i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02}:{:02}",
        seconds / 3600 % 24,
        seconds / 60 % 60,
        seconds % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn dynamic_html_is_escaped_and_credential_labels_are_redacted() {
        assert_eq!(
            e("<script a=\"x\">&'</script>"),
            "&lt;script a=&quot;x&quot;&gt;&amp;&#39;&lt;/script&gt;"
        );
        assert_eq!(
            display("failure https://example.invalid/file?token=secret"),
            "[redacted]"
        );
        assert_eq!(display("safe <title>"), "safe &lt;title&gt;");
    }
    #[test]
    fn civil_timestamps_cover_epoch_leap_day_and_upper_limit() {
        assert_eq!(timestamp(0), "1970-01-01 00:00:00");
        assert_eq!(timestamp(1_709_164_800), "2024-02-29 00:00:00");
        assert_eq!(timestamp(253_402_300_799), "9999-12-31 23:59:59");
        assert_eq!(timestamp(u64::MAX), u64::MAX.to_string());
    }
}
