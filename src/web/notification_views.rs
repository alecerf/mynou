//! Original escaped notification reports and session-bound decisions.
use super::{
    forms::{Form, decimal},
    session::Session,
    views::{array, e, form, frame, hidden, scalar, text},
};
use crate::{Result, engine::Engine, json::Value};
use std::sync::Arc;
pub(super) fn list(engine: &Arc<Engine>, session: &Session, query: &Form) -> Result<String> {
    query.only(&["offset", "limit"])?;
    let offset = decimal(query.value("offset")?, 2048, "offset")? as usize;
    let limit = if query.value("limit")?.is_empty() {
        50
    } else {
        decimal(query.value("limit")?, 200, "limit")? as usize
    };
    let report = engine.notifications(offset, limit)?;
    let mut body = String::from(
        "<section class=panel><h1>Notifications</h1><p>Configured routes receive recorded outcomes. Delivery retries keep the same event ID; receivers should deduplicate repeated IDs.</p><h2>Routes</h2><table><tr><th>Route</th><th>Scope</th><th>Enabled</th></tr>",
    );
    for r in array(report.get("routes").unwrap_or(&Value::Null)) {
        body.push_str(&format!(
            "<tr><td>{}</td><td>{}: {}</td><td>{}</td></tr>",
            e(text(r, "id")),
            e(text(r, "kind")),
            e(text(r, "scope_id")),
            scalar(r, "enabled")
        ));
    }
    body.push_str("</table>");
    body.push_str(&form("/ui/notifications/dispatch", session));
    body.push_str("<button>Deliver due events</button></form><h2>Recorded deliveries</h2><table><tr><th>Route</th><th>Scope</th><th>Outcome</th><th>Delivery</th><th>Attempts</th><th>Actions</th></tr>");
    for event in array(report.get("events").unwrap_or(&Value::Null)) {
        body.push_str(&format!(
            "<tr><td>{}</td><td>{}: {}</td><td>{}</td><td>{}</td><td>{}</td><td>",
            e(text(event, "route_id")),
            e(text(event, "kind")),
            e(text(event, "scope_id")),
            e(text(event, "outcome")),
            e(text(event, "phase")),
            scalar(event, "attempts")
        ));
        if matches!(text(event, "phase"), "pending" | "failed") {
            body.push_str(&form("/ui/notifications/control", session));
            body.push_str(&hidden("kind", text(event, "kind")));
            body.push_str(&hidden("event_id", text(event, "id")));
            let max = array(report.get("routes").unwrap_or(&Value::Null))
                .iter()
                .find(|r| text(r, "id") == text(event, "route_id"))
                .and_then(|r| r.get("max_attempts"))
                .and_then(Value::as_u64)
                .unwrap_or(0);
            if event.get("attempts").and_then(Value::as_u64).unwrap_or(8) < max {
                body.push_str("<button name=action value=retry>Review retry</button>");
            }
            body.push_str("<button name=action value=discard>Review discard</button></form>");
        }
        body.push_str("</td></tr>");
    }
    body.push_str("</table>");
    if offset > 0 {
        body.push_str(&format!(
            "<p><a href=\"/ui/notifications?offset={}&amp;limit={limit}\">Previous</a></p>",
            offset.saturating_sub(limit)
        ));
    }
    if offset + limit < report.get("count").and_then(Value::as_u64).unwrap_or(0) as usize {
        body.push_str(&format!(
            "<p><a href=\"/ui/notifications?offset={}&amp;limit={limit}\">Next</a></p>",
            offset + limit
        ));
    }
    body.push_str("</section>");
    Ok(frame(
        "Notifications",
        "/ui/notifications",
        Some(session),
        &body,
    ))
}
pub(super) fn review(session: &Session, report: &Value) -> String {
    let event = report.get("event").unwrap_or(&Value::Null);
    let mut body = format!(
        "<section class=panel><h1>Review notification {}</h1><p>Route: {}. Outcome: {}. Attempts already used: {}. Retrying keeps the original event ID and attempt budget. Discarding only changes delivery.</p>",
        e(text(report, "action")),
        e(text(event, "route_id")),
        e(text(event, "outcome")),
        scalar(event, "attempts")
    );
    body.push_str(&form("/ui/notifications/control", session));
    for (k, v) in [
        ("kind", text(report, "kind")),
        ("event_id", text(event, "id")),
        ("action", text(report, "action")),
        ("plan_id", text(report, "plan_id")),
        ("apply", "yes"),
    ] {
        body.push_str(&hidden(k, v));
    }
    body.push_str("<button>Apply reviewed delivery decision</button></form></section>");
    frame(
        "Review notification",
        "/ui/notifications",
        Some(session),
        &body,
    )
}
