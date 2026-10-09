//! Native series settings and date-window calendar, using the common escaped shell.
use super::{
    forms::{Form, decimal, encode},
    session::Session,
    views::{
        array, badge, bulk_controls, checkbox, detail_pager, display, e, flag, form, frame, hidden,
        scalar, text,
    },
};
use crate::{Result, engine::Engine, series::CalendarQuery};
use std::sync::Arc;

pub fn list(engine: &Arc<Engine>, session: &Session, query: &Form) -> Result<String> {
    query.only(&["page"])?;
    let page = page_number(query)?;
    let entries = engine.series()?;
    let mut body = String::from(
        "<p class=lead>Keep following a series as its catalog and air dates change.</p><details class=panel><summary>Track a series</summary>",
    );
    body.push_str(&form("/ui/series/track", session));
    body.push_str(&hidden("kind", "series"));
    body.push_str("<div class=fields><div class=wide><label for=title>Series title</label><input id=title name=title required maxlength=4096></div><div><label for=year>First air year</label><input id=year name=year type=number min=0 max=9999></div><div><label for=tmdb_id>TMDB series ID (optional)</label><input id=tmdb_id name=tmdb_id type=number min=1 max=9007199254740991></div><div><label for=season>Season (empty means all)</label><input id=season name=season type=number min=0 max=9999></div></div><label><input type=checkbox name=unmonitored value=true> Save without automatic episode acquisition</label><label><input type=checkbox name=future_only value=true> Monitor from today onward</label><label><input type=checkbox name=include_specials value=true> Include catalog specials (season zero)</label><button type=submit>Track series</button></form><p class=muted>TMDB must be enabled. Aired missing episodes are queued in bounded batches. Unknown air dates remain unresolved.</p></details>");
    body.push_str(&form("/ui/series/action", session));
    body.push_str("<div class=table-wrap><table><caption>Tracked series scopes</caption><thead><tr><th scope=col>Select</th><th scope=col>Series</th><th scope=col>Scope</th><th scope=col>Monitoring</th><th scope=col>Episodes / undated</th><th scope=col>Catalog</th></tr></thead><tbody>");
    let values = array(&entries);
    let page = page.min(values.len().div_ceil(50).max(1));
    for entry in values.iter().skip((page - 1) * 50).take(50) {
        let id = text(entry, "id");
        let request = entry.get("request").unwrap_or(&crate::json::Value::Null);
        body.push_str(&format!("<tr><td>{}</td><td><a href=\"/ui/series/{}\">{}</a><small>{}</small></td><td>{}</td><td>{}</td><td>{} / {}</td><td>{}</td></tr>", checkbox(id), e(id), display(text(request, "title")), scalar(request, "year"),
            if request.get("season").and_then(crate::json::Value::as_u64).unwrap_or(0) == 0 { "All seasons".into() } else { format!("Season {}", scalar(request, "season")) },
            if flag(entry, "monitored") { "On" } else { "Off" }, scalar(entry, "episode_count"), scalar(entry, "undated_count"),
            if entry.get("last_error").and_then(crate::json::Value::as_str).is_some() { "Needs attention" } else { "Plan available" }));
    }
    body.push_str("</tbody></table></div>");
    body.push_str(&bulk_controls(&[
        ("monitor", "Monitor selected"),
        ("unmonitor", "Unmonitor selected"),
    ]));
    body.push_str("</form>");
    body.push_str(&detail_pager(
        "/ui/series",
        "page",
        values.len(),
        page,
        "",
        1,
    ));
    Ok(frame("Series", "/ui/series", Some(session), &body))
}

pub fn detail(engine: &Arc<Engine>, session: &Session, query: &Form, id: &str) -> Result<String> {
    query.only(&["page"])?;
    let page = page_number(query)?;
    let value = engine.series_record(id)?;
    let request = value.get("request").unwrap_or(&crate::json::Value::Null);
    let mut body = format!(
        "<p class=lead>{}</p><section class=panel><h2>Monitoring settings</h2><p>Episode choices inherit these series settings. Disabling monitoring retains existing requests and files.</p>",
        display(text(request, "title"))
    );
    if let Some(error) = value.get("last_error").and_then(crate::json::Value::as_str) {
        body.push_str(&format!(
            "<p class=notice role=alert>{}</p>",
            display(error)
        ));
    }
    body.push_str(&form("/ui/series/settings", session));
    body.push_str(&hidden("id", id));
    for (name, label, enabled) in [
        ("enabled", "Monitoring", flag(&value, "monitored")),
        (
            "include_specials",
            "Include catalog specials",
            flag(&value, "include_specials"),
        ),
    ] {
        body.push_str(&format!("<label for={name}>{label}</label><select id={name} name={name}><option value=true{}>On</option><option value=false{}>Off</option></select>", if enabled { " selected" } else { "" }, if enabled { "" } else { " selected" }));
    }
    body.push_str(&format!("<label for=start_date>Earliest monitored air date (empty means all)</label><input id=start_date name=start_date type=date value=\"{}\"><button type=submit>Save settings</button></form>", e(text(&value, "start_date"))));
    body.push_str(&form("/ui/series/refresh", session));
    body.push_str(&hidden("id", id));
    body.push_str("<button type=submit class=secondary>Refresh catalog and queue aired episodes</button></form>");
    body.push_str(&format!(
        "<p><a href=\"/ui/calendar?series_id={}\">Open episode calendar</a></p></section>",
        e(id)
    ));
    body.push_str("<section class=panel><h2>Automatic season-pack search</h2><p>Search and rank packs, then inspect their metadata without downloading video. Exact episode markers must cover every missing aired episode allowed by your specials, date and exclusion choices. This on-demand action also works while background monitoring is off.</p>");
    body.push_str(&form("/ui/series/pack-search", session));
    body.push_str(&hidden("id", id));
    body.push_str("<label for=pack_season>Catalog season (0 for allowed specials)</label><input id=pack_season name=season type=number min=0 max=9999 required value=1><button type=submit name=action value=preview>Preview season packs</button></form></section>");
    body.push_str("<details class=panel><summary>Acquire a mapped pack</summary><p>Choose one torrent source and map its exact video paths to already aired catalog episodes. Include the torrent's top-level directory in each path. New transfers select mapped files and their boundary pieces; existing full transfers keep their policy. Existing episode requests are reused.</p>");
    body.push_str(&form("/ui/series/packs", session));
    body.push_str(&hidden("id", id));
    body.push_str("<label for=source_value>Magnet, torrent URL or server torrent path</label><input id=source_value name=source_value required maxlength=8192 autocomplete=off><label for=episodes>Episode mappings (JSON array)</label><textarea id=episodes name=episodes required rows=6 maxlength=8192 placeholder='[{&quot;season&quot;:1,&quot;episode&quot;:1,&quot;file_path&quot;:&quot;Pack/001.mp4&quot;}]'></textarea><p class=muted>Mappings require a different file for each episode. Review the catalog and file contents before submitting. This explicit action can acquire episodes excluded from automatic monitoring.</p><button type=submit>Acquire mapped episodes</button></form></details>");
    body.push_str("<details class=panel><summary>One video for multiple episodes</summary><p>Explicitly map one video to consecutive catalog episodes in one season. Preview authenticates metadata before recording all owners together. One library file is retained for the whole group. Individual upgrades are blocked until coordinated group replacement is available.</p>");
    body.push_str(&form("/ui/series/shared-file", session));
    body.push_str(&hidden("id", id));
    body.push_str("<label for=shared_source>Magnet, torrent URL or server torrent path</label><input id=shared_source name=source_value required maxlength=8192 autocomplete=off><label for=shared_path>Exact video path including the top-level directory</label><input id=shared_path name=file_path required maxlength=4096><label for=shared_season>Canonical season</label><input id=shared_season name=season type=number min=0 max=9999 required value=1><label for=shared_episodes>Canonical episode numbers (JSON array)</label><textarea id=shared_episodes name=episodes required maxlength=8192 rows=2 placeholder='[1,2]'></textarea><button type=submit name=action value=preview>Preview shared file</button></form></details>");
    let episodes = value.get("episodes").map(array).unwrap_or_default();
    body.push_str("<details class=panel><summary>Episode numbering</summary><p>Keep each episode's library number while choosing the labels used by your sources. Review changed catalog labels before accepting them. Existing requests keep their saved choices.</p>");
    body.push_str(&form("/ui/series/numbering", session));
    body.push_str(&hidden("id", id));
    body.push_str("<label for=numbering_changes>Numbering choices (JSON array)</label><textarea id=numbering_changes name=changes required rows=5 maxlength=8192>[]</textarea><p class=muted>Use catalog_id, catalog season/episode, and source season/episode or absolute. An empty array shows the current catalog comparison.</p><button type=submit name=action value=preview>Preview numbering</button></form></details>");
    let page = page.min(episodes.len().div_ceil(50).max(1));
    body.push_str("<div class=table-wrap><table><caption>Catalog episode plan</caption><thead><tr><th scope=col>Episode</th><th scope=col>Title</th><th scope=col>Air date (UTC day)</th><th scope=col>Monitoring</th><th scope=col>Episode choice</th></tr></thead><tbody>");
    for episode in episodes.iter().skip((page - 1) * 50).take(50) {
        body.push_str(&format!(
            "<tr><td>S{} E{}</td><td>{}</td><td>{}</td><td>{}</td><td>",
            scalar(episode, "season"),
            scalar(episode, "episode"),
            display(text(episode, "title")),
            episode
                .get("air_date")
                .and_then(crate::json::Value::as_str)
                .map_or_else(|| "Unknown air date".into(), display),
            if flag(episode, "monitored") {
                "On"
            } else {
                "Off"
            }
        ));
        if episode
            .get("catalog_id")
            .is_none_or(|value| matches!(value, crate::json::Value::Null))
        {
            body.push_str("<small>Catalog identity required for automatic acquisition</small>");
        }
        body.push_str(&form("/ui/series/episodes", session));
        body.push_str(&hidden("id", id));
        body.push_str(&hidden("season", &scalar(episode, "season")));
        body.push_str(&hidden("episode", &scalar(episode, "episode")));
        body.push_str(&format!("<button type=submit name=enabled value={} class=secondary>{}</button></form></td></tr>", if flag(episode, "excluded") { "true" } else { "false" }, if flag(episode, "excluded") { "Include episode" } else { "Exclude episode" }));
    }
    body.push_str("</tbody></table></div>");
    body.push_str(&detail_pager(
        &format!("/ui/series/{id}"),
        "page",
        episodes.len(),
        page,
        "",
        1,
    ));
    Ok(frame("Series details", "/ui/series", Some(session), &body))
}

pub fn shared_file(session: &Session, id: &str, report: &crate::json::Value) -> String {
    let binding = report.get("binding").unwrap_or(&crate::json::Value::Null);
    let mut body = format!(
        "<p><a href=\"/ui/series/{}\">Back to series</a></p><p class=lead>Review shared video ownership</p><section class=panel><h2>One physical file</h2><p>Torrent: {}</p><p>Exact source path: {}</p><p>Canonical owners: S{} E{} through E{}</p><p>Single library destination: {}</p><p>{} new owner(s). Each requested episode confirms this exact path in Plex. Cancellation retains the full ownership range and any imported file. Individual upgrades are blocked.</p></section>",
        e(id),
        display(text(binding, "torrent_id")),
        display(text(binding, "file_path")),
        scalar(binding, "season"),
        scalar(binding, "first_episode"),
        scalar(binding, "last_episode"),
        display(text(binding, "import_path")),
        scalar(report, "new_owners")
    );
    body.push_str(&form("/ui/series/shared-file", session));
    for (name, value) in [("id", id), ("plan_id", text(report, "plan_id"))] {
        body.push_str(&hidden(name, value));
    }
    body.push_str("<p class=muted>This review expires in ten minutes and is tied to this browser session. Preview again after changing the source or owners.</p>");
    body.push_str("<button type=submit name=action value=apply>Record reviewed shared ownership</button></form>");
    frame("Shared video preview", "/ui/series", Some(session), &body)
}

pub fn numbering(session: &Session, id: &str, report: &crate::json::Value) -> String {
    let mut body = format!(
        "<p><a href=\"/ui/series/{}\">Back to series</a></p><p class=lead>Review episode numbering</p><p>Library numbers remain fixed. These choices apply to future requests; existing downloads and imported files retain their saved identity.</p>",
        e(id)
    );
    for issue in array(report.get("issues").unwrap_or(&crate::json::Value::Null)) {
        body.push_str(&format!(
            "<p class=notice role=alert>{}</p>",
            display(issue.as_str().unwrap_or("Unresolved numbering"))
        ));
    }
    body.push_str("<div class=table-wrap><table><caption>Known episode numbering</caption><thead><tr><th scope=col>Catalog ID</th><th scope=col>Title</th><th scope=col>Library number</th><th scope=col>Current catalog number</th><th scope=col>Proposed source label</th></tr></thead><tbody>");
    let episodes = array(report.get("episodes").unwrap_or(&crate::json::Value::Null));
    for ep in episodes.iter().take(100) {
        let number = |key| {
            ep.get(key).map_or_else(String::new, |number| {
                if number.get("absolute").is_some() {
                    format!("Absolute {}", scalar(number, "absolute"))
                } else {
                    format!(
                        "S{} E{}",
                        scalar(number, "season"),
                        scalar(number, "episode")
                    )
                }
            })
        };
        body.push_str(&format!(
            "<tr><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td></tr>",
            scalar(ep, "catalog_id"),
            display(text(ep, "title")),
            e(&number("canonical")),
            e(&number("catalog")),
            e(&number("source"))
        ));
    }
    body.push_str("</tbody></table></div>");
    if episodes.len() > 100 {
        body.push_str("<p class=muted>The first 100 known episodes are shown. Use the CLI or API for the complete comparison.</p>");
    }
    let changes =
        crate::json::stringify(report.get("changes").unwrap_or(&crate::json::Value::Null));
    body.push_str(&form("/ui/series/numbering", session));
    body.push_str(&hidden("id", id));
    body.push_str(&format!("<label for=changes>Revise choices</label><textarea id=changes name=changes required rows=6 maxlength=8192>{}</textarea><button type=submit name=action value=preview>Preview revised choices</button></form>",e(&changes)));
    if flag(report, "resolved") {
        body.push_str(&form("/ui/series/numbering", session));
        body.push_str(&hidden("id", id));
        body.push_str(&hidden("changes", &changes));
        body.push_str(&hidden("plan_id", text(report, "plan_id")));
        body.push_str(
            "<button type=submit name=action value=apply>Save reviewed numbering</button></form>",
        );
    }
    frame("Episode numbering", "/ui/series", Some(session), &body)
}

pub fn pack_search(session: &Session, id: &str, report: &crate::json::Value) -> String {
    let mut body = format!(
        "<p><a href=\"/ui/series/{}\">Back to series</a></p><p class=lead>Season {} pack decisions</p><p>Preview inspects metadata without recording requests or downloading media. Acquisition searches again and checks this scope, candidate, torrent hash and file mapping.</p>",
        e(id),
        scalar(report, "season")
    );
    if flag(report, "scope_empty") {
        body.push_str("<p class=notice>No missing aired episodes are eligible. Existing requests, including failed or cancelled requests, remain retained.</p>");
    }
    if let Some(candidate) = report
        .get("selected_candidate_id")
        .and_then(crate::json::Value::as_str)
    {
        body.push_str("<section class=panel><h2>Resolved episode files</h2><div class=table-wrap><table><caption>Verified metadata mapping</caption><thead><tr><th scope=col>Episode</th><th scope=col>Catalog title</th><th scope=col>Torrent file</th></tr></thead><tbody>");
        for row in report.get("mapping").map(array).unwrap_or_default() {
            body.push_str(&format!(
                "<tr><td>S{}E{}</td><td>{}</td><td>{}</td></tr>",
                scalar(row, "season"),
                scalar(row, "episode"),
                display(text(row, "title")),
                display(text(row, "file_path"))
            ));
        }
        body.push_str("</tbody></table></div>");
        body.push_str(&form("/ui/series/pack-search", session));
        for (key, value) in [
            ("id", id),
            ("season", &scalar(report, "season")),
            ("scope_id", text(report, "scope_id")),
            ("candidate_id", candidate),
        ] {
            body.push_str(&hidden(key, value));
        }
        body.push_str("<button type=submit name=action value=apply>Acquire resolved pack</button></form></section>");
    } else if !flag(report, "scope_empty") {
        body.push_str("<p class=notice>No candidate resolved every requested episode within the inspection limits. Review the decisions or use an explicit mapping.</p>");
    }
    body.push_str("<section class=panel><h2>Metadata decisions</h2><ul>");
    for decision in report
        .get("metadata_decisions")
        .map(array)
        .unwrap_or_default()
    {
        body.push_str(&format!(
            "<li>{}: {} {}</li>",
            display(text(decision, "candidate_id")),
            display(text(decision, "status")),
            display(text(decision, "reason"))
        ));
    }
    body.push_str("</ul></section><section class=panel><h2>Ranked title assessments</h2><p>Title acceptance precedes metadata decisions. At most eight candidates are inspected; this page shows up to 50 title assessments in each group.</p>");
    for key in ["accepted", "rejected"] {
        body.push_str(&format!(
            "<h3>{}</h3><ul>",
            if key == "accepted" {
                "Accepted titles"
            } else {
                "Rejected titles"
            }
        ));
        for candidate in report
            .get(key)
            .map(array)
            .unwrap_or_default()
            .iter()
            .take(50)
        {
            body.push_str(&format!(
                "<li>{} <small>{}</small>",
                display(text(candidate, "title")),
                display(text(candidate, "source"))
            ));
            if let Some(assessment) = candidate.get("assessment") {
                for reason in assessment.get("reasons").map(array).unwrap_or_default() {
                    if let Some(reason) = reason.as_str() {
                        body.push_str(&format!("<small>{}</small>", display(reason)));
                    }
                }
            }
            body.push_str("</li>");
        }
        body.push_str("</ul>");
    }
    body.push_str("</section>");
    frame("Season-pack search", "/ui/series", Some(session), &body)
}

pub fn calendar_export(engine: &Arc<Engine>, fields: &Form) -> Result<String> {
    fields.only(&["from", "to", "series_id"])?;
    let mut query = CalendarQuery::new(Some(fields.value("from")?), Some(fields.value("to")?))?;
    let id = fields.value("series_id")?;
    query.series_id = (!id.is_empty()).then(|| id.to_owned());
    engine.episode_calendar_ics(&query)
}

pub fn calendar(engine: &Arc<Engine>, session: &Session, fields: &Form) -> Result<String> {
    fields.only(&["from", "to", "series_id", "page"])?;
    let page = page_number(fields)?;
    let mut query = CalendarQuery::new(Some(fields.value("from")?), Some(fields.value("to")?))?;
    let id = fields.value("series_id")?;
    query.series_id = (!id.is_empty()).then(|| id.to_owned());
    query.offset = (page - 1) * 50;
    query.validate()?;
    let value = engine.episode_calendar(&query)?;
    let mut body = format!(
        "<p class=lead>Known episode air dates, monitoring choices and request state.</p><form method=get action=/ui/calendar class=filters><div><label for=from>From</label><input id=from name=from type=date required value=\"{}\"></div><div><label for=to>To</label><input id=to name=to type=date required value=\"{}\"></div>{}<button type=submit>Show dates</button></form><p class=muted>Dates use the catalog's UTC day, not an exact local premiere time. Unknown dates are listed in series details. Windows are limited to 367 days.</p>",
        e(&query.from),
        e(&query.to),
        hidden("series_id", id)
    );
    body.push_str("<div class=table-wrap><table><caption>Episode calendar</caption><thead><tr><th scope=col>Air date</th><th scope=col>Series</th><th scope=col>Episode</th><th scope=col>Title</th><th scope=col>Monitoring</th><th scope=col>State</th></tr></thead><tbody>");
    for episode in value.get("episodes").map(array).unwrap_or_default() {
        let state = episode
            .get("job_id")
            .and_then(crate::json::Value::as_str)
            .map_or_else(
                || badge(text(episode, "state")),
                |id| {
                    format!(
                        "<a href=\"/ui/jobs/{}\">{}</a>",
                        e(id),
                        badge(text(episode, "state"))
                    )
                },
            );
        body.push_str(&format!("<tr><td>{}</td><td><a href=\"/ui/series/{}\">{}</a></td><td>S{} E{}</td><td>{}</td><td>{}</td><td>{state}</td></tr>", display(text(episode, "air_date")), e(text(episode, "series_id")), display(text(episode, "title")), scalar(episode, "season"), scalar(episode, "episode"), display(text(episode, "episode_title")), if flag(episode, "monitored") { "On" } else { "Off" }));
    }
    body.push_str("</tbody></table></div>");
    let total = value
        .get("total")
        .and_then(crate::json::Value::as_u64)
        .unwrap_or(0) as usize;
    body.push_str("<section class=panel><h2>Export to your calendar</h2>");
    if total == 0 {
        body.push_str("<p class=muted>No known dates in this window, so there is nothing to export.</p>");
    } else if total > 200 {
        body.push_str(&format!("<p class=muted>This window has {total} known dates; one file holds at most 200. Narrow the dates or choose one series to export.</p>"));
    } else {
        body.push_str(&format!(
            "<p><a href=\"/ui/calendar.ics?from={}&amp;to={}&amp;series_id={}\" download>Download all {total} dates as an .ics file</a></p>",
            encode(&query.from),
            encode(&query.to),
            encode(id)
        ));
    }
    body.push_str("<p class=muted>The file is a private download of known dates in this window, not a subscription. Events are all-day UTC catalog dates that may change; a later export keeps the same event identities. Unknown dates are not included. Compatibility with specific calendar apps is not claimed.</p></section>");
    body.push_str(&format!(
        "<nav class=pager aria-label=Pagination><span>{total} episodes · Page {page}</span>"
    ));
    let link = |page| {
        format!(
            "/ui/calendar?from={}&amp;to={}&amp;series_id={}&amp;page={page}",
            encode(&query.from),
            encode(&query.to),
            encode(id)
        )
    };
    if page > 1 {
        body.push_str(&format!(
            "<a href=\"{}\" rel=prev>Previous</a>",
            link(page - 1)
        ));
    }
    if page * 50 < total {
        body.push_str(&format!("<a href=\"{}\" rel=next>Next</a>", link(page + 1)));
    }
    body.push_str("</nav>");
    Ok(frame(
        "Episode calendar",
        "/ui/calendar",
        Some(session),
        &body,
    ))
}

fn page_number(form: &Form) -> Result<usize> {
    let text = form.value("page")?;
    if text.is_empty() {
        return Ok(1);
    }
    let page = decimal(text, 401, "Page")? as usize;
    if page == 0 {
        return Err("Page numbering starts at one".into());
    }
    Ok(page)
}
