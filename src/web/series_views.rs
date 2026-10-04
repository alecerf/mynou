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
    body.push_str("<details class=panel><summary>Acquire a mapped pack</summary><p>Choose one torrent source and map its exact video paths to already aired catalog episodes. Include the torrent's top-level directory in each path. The full torrent downloads once; only mapped files are imported. Existing episode requests are reused.</p>");
    body.push_str(&form("/ui/series/packs", session));
    body.push_str(&hidden("id", id));
    body.push_str("<label for=source_url>Magnet, torrent URL or server torrent path</label><input id=source_url name=source_url required maxlength=8192 autocomplete=off><label for=episodes>Episode mappings (JSON array)</label><textarea id=episodes name=episodes required rows=6 maxlength=8192 placeholder='[{&quot;season&quot;:1,&quot;episode&quot;:1,&quot;file_path&quot;:&quot;Pack/001.mp4&quot;}]'></textarea><p class=muted>Mappings require a different file for each episode. Review the catalog and file contents before submitting. This explicit action can acquire episodes excluded from automatic monitoring.</p><button type=submit>Acquire mapped episodes</button></form></details>");
    let episodes = value.get("episodes").map(array).unwrap_or_default();
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
