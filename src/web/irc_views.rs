//! Source health and unverified announcement reviews, with escaped HTML.
use super::{
    forms::{Form, decimal},
    session::Session,
    views::{array, display, e, form, frame, hidden, scalar, text},
};
use crate::{Result, engine::Engine, json::Value};
use std::sync::Arc;
pub(super) fn list(engine: &Arc<Engine>, session: &Session, query: &Form) -> Result<String> {
    query.only(&["offset", "limit"])?;
    let offset = decimal(query.value("offset")?, 1000, "offset")? as usize;
    let limit = if query.value("limit")?.is_empty() {
        50
    } else {
        decimal(query.value("limit")?, 200, "limit")? as usize
    };
    let sources = engine.irc_sources()?;
    let report = engine.irc_announcements(offset, limit)?;
    let mut body = String::from(
        "<section class=panel><h1>IRC announcements</h1><p>Receive configured announcements, inspect filters and record audit reviews. Explicit grab rules route verified candidates to existing approved requests. Reviews do not authorize downloads.</p><h2>Sources</h2><table><thead><tr><th>Source</th><th>Channel</th><th>Format</th><th>Authentication</th><th>Connection</th><th>Received</th><th>Duplicates</th></tr></thead><tbody>",
    );
    for s in array(sources.get("sources").unwrap_or(&Value::Null)) {
        let h = s.get("health").unwrap_or(&Value::Null);
        body.push_str(&format!(
            "<tr><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{} {}</td><td>{}</td><td>{}</td></tr>",
            e(text(s, "id")),
            e(text(s, "channel")),
            e(text(s, "announcement_format")),
            if text(s, "authentication") == "sasl_plain" {
                if s.get("health")
                    .and_then(|h| h.get("sasl_authenticated"))
                    .and_then(Value::as_bool)
                    == Some(true)
                {
                    "SASL authenticated"
                } else {
                    "SASL required"
                }
            } else {
                "No SASL"
            },
            display(text(h, "phase")),
            display(text(h, "last_error")),
            scalar(h, "received"),
            scalar(h, "duplicates")
        ));
    }
    body.push_str("</tbody></table><h2>Configured rules</h2><p>Edit source and rule settings in your configuration, then restart the service.</p><table><thead><tr><th>Rule</th><th>Source</th><th>Kind</th><th>Profile</th><th>Action</th><th>Enabled</th></tr></thead><tbody>");
    for r in array(sources.get("rules").unwrap_or(&Value::Null)) {
        body.push_str(&format!(
            "<tr><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td></tr>",
            e(text(r, "id")),
            e(text(r, "source")),
            e(text(r, "kind")),
            e(text(r, "profile")),
            e(text(r, "action")),
            scalar(r, "enabled")
        ));
    }
    body.push_str("</tbody></table></section><section class=panel><h2>Announcement history</h2><table><thead><tr><th>Release</th><th>Source</th><th>Filters</th><th>Routing</th><th>Review</th></tr></thead><tbody>");
    for r in array(report.get("announcements").unwrap_or(&Value::Null)) {
        let a = r.get("announcement").unwrap_or(&Value::Null);
        body.push_str(&format!(
            "<tr><td><a href=\"/ui/irc/{}\">{}</a></td><td>{}</td><td>{}</td><td>{}</td><td>{}</td></tr>",
            e(text(r, "id")),
            display(text(a, "title")),
            e(text(r, "source_id")),
            e(text(r, "outcome")),
            e(text(r, "routing_outcome")),
            e(text(r, "decision"))
        ));
    }
    body.push_str("</tbody></table>");
    let total = report.get("total").and_then(Value::as_u64).unwrap_or(0) as usize;
    if offset > 0 {
        body.push_str(&format!(
            "<a href=\"/ui/irc?offset={}&amp;limit={}\">Previous announcements</a> ",
            offset.saturating_sub(limit),
            limit
        ));
    }
    if offset + limit < total {
        body.push_str(&format!(
            "<a href=\"/ui/irc?offset={}&amp;limit={}\">Next announcements</a>",
            offset + limit,
            limit
        ));
    }
    body.push_str("</section>");
    Ok(frame("IRC announcements", "/ui/irc", Some(session), &body))
}
pub(super) fn detail(engine: &Arc<Engine>, session: &Session, id: &str) -> Result<String> {
    let r = engine.irc_announcement(id)?;
    let mut body = describe(&r);
    if text(&r, "decision") == "pending" {
        body.push_str(&form("/ui/irc/control", session));
        body.push_str(&hidden("id", id));
        body.push_str("<button name=action value=acknowledge>Review acknowledgement</button><button name=action value=dismiss>Review dismissal</button></form>");
    }
    body.push_str("</section>");
    Ok(frame(
        "Announcement details",
        "/ui/irc",
        Some(session),
        &body,
    ))
}
fn describe(r: &Value) -> String {
    let a = r.get("announcement").unwrap_or(&Value::Null);
    let mut body = format!(
        "<p><a href=/ui/irc>All announcements</a></p><section class=panel><h1>{}</h1><p>Source: {}. Filter result: {}. Decision: {}.</p><p>Claimed media: {} ({}) S{}E{}. TMDB ID: {}.</p><p>Claimed torrent hash: {}.</p><p>Catalog identity remains a source claim. Automatic routing verifies torrent metadata against an existing approved request. Acknowledgement and dismissal are audit decisions; use the linked job to cancel acquisition.</p><table><thead><tr><th>Rule</th><th>Profile</th><th>Outcome</th></tr></thead><tbody>",
        display(text(a, "title")),
        e(text(r, "source_id")),
        e(text(r, "outcome")),
        e(text(r, "decision")),
        display(text(a, "media_title")),
        scalar(a, "year"),
        scalar(a, "season"),
        scalar(a, "episode"),
        scalar(a, "tmdb_id"),
        e(text(a, "info_hash"))
    );
    for v in array(r.get("evaluations").unwrap_or(&Value::Null)) {
        body.push_str(&format!(
            "<tr><td>{}</td><td>{}</td><td>{}</td></tr>",
            e(text(v, "rule_id")),
            e(text(v, "profile")),
            e(text(v, "outcome"))
        ));
    }
    body.push_str("</tbody></table>");
    body.push_str(&format!(
        "<h2>Acquisition routing</h2><p>Outcome: {}.</p>",
        e(text(r, "routing_outcome"))
    ));
    if let Some(route) = r.get("route") {
        let origin = route.get("origin").unwrap_or(&Value::Null);
        if !text(origin, "job_id").is_empty() {
            body.push_str(&format!("<p><a href=\"/ui/jobs/{}\">View associated job</a></p><p>Verified file: {}. Size: {} bytes.</p>",
                e(text(origin, "job_id")), display(text(origin, "file")), scalar(origin, "file_length")));
        }
    }
    body
}
pub(super) fn review(session: &Session, id: &str, report: &Value) -> String {
    let r = report.get("announcement").unwrap_or(&Value::Null);
    let mut body = describe(r);
    body.push_str(&format!("<h2>Review {}</h2>", e(text(report, "action"))));
    body.push_str(&form("/ui/irc/control", session));
    for (k, v) in [
        ("id", id),
        ("action", text(report, "action")),
        ("plan_id", text(report, "plan_id")),
        ("apply", "yes"),
    ] {
        body.push_str(&hidden(k, v));
    }
    body.push_str("<button>Apply reviewed decision</button></form></section>");
    frame("Review announcement", "/ui/irc", Some(session), &body)
}
