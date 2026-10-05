//! Operator-only requester policy and approval pages.
use super::{
    forms::{Form, decimal},
    session::Session,
    views::{array, display, e, form, frame, hidden, scalar, text},
};
use crate::{
    Result,
    engine::Engine,
    json::Value,
    requesters::{ControlRequest, Policy},
};
use std::sync::Arc;

pub(super) fn list(engine: &Arc<Engine>, session: &Session) -> Result<String> {
    let report = engine.requesters()?;
    let mut body = String::from(
        "<section class=panel><h1>Plex requesters</h1><p>Each account has its own policy, approvals and quota. Compatible requests share acquisition. Removing one account's demand retains other interests and imported media.</p>",
    );
    body.push_str(&form("/ui/requesters/sync", session));
    body.push_str("<button>Poll accounts</button></form><table><thead><tr><th>Account</th><th>Policy</th><th>Active</th><th>Today</th><th>Pending</th><th>Poll result</th></tr></thead><tbody>");
    for account in array(report.get("accounts").unwrap_or(&Value::Null)) {
        body.push_str(&format!("<tr><td><a href=\"/ui/requesters/{}\">{}</a></td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td></tr>",e(text(account,"id")),e(text(account,"id")),if account.get("policy").and_then(|p|p.get("enabled")).and_then(Value::as_bool)==Some(true){"Enabled"}else{"Disabled"},scalar(account,"active"),scalar(account,"daily"),scalar(account,"pending"),display(text(account,"last_error"))));
    }
    body.push_str("</tbody></table></section>");
    Ok(frame(
        "Plex requesters",
        "/ui/requesters",
        Some(session),
        &body,
    ))
}
fn select(
    name: &str,
    label: &str,
    current: &str,
    values: impl IntoIterator<Item = String>,
) -> String {
    let mut html = format!("<label>{}<select name={name}>", e(label));
    for v in values {
        html.push_str(&format!(
            "<option value=\"{}\"{}>{}</option>",
            e(&v),
            if v == current { " selected" } else { "" },
            e(&v)
        ));
    }
    html.push_str("</select></label>");
    html
}
pub(super) fn detail(
    engine: &Arc<Engine>,
    session: &Session,
    query: &Form,
    id: &str,
) -> Result<String> {
    query.only(&["offset", "limit"])?;
    let offset = decimal(query.value("offset")?, 10_000, "offset")? as usize;
    let limit = if query.value("limit")?.is_empty() {
        100
    } else {
        decimal(query.value("limit")?, 200, "limit")? as usize
    };
    let report = engine.requester(id, offset, limit)?;
    let policy = Policy::from_json(report.get("policy").ok_or("Missing requester policy")?)?;
    let mut body = format!(
        "<p><a href=/ui/requesters>All requesters</a></p><section class=panel><h1>Requester {}</h1><p>Active acquisitions: {}. Requests charged today (UTC): {}. Pending approvals: {}.</p><p>Watchlist cursor: {}. Last poll: {}. {}</p>",
        e(id),
        scalar(&report, "active"),
        scalar(&report, "daily"),
        scalar(&report, "pending"),
        scalar(&report, "cursor"),
        scalar(&report, "polled_at"),
        display(text(&report, "last_error"))
    );
    if report.get("configured").and_then(Value::as_bool) == Some(true) {
        body.push_str(&form("/ui/requesters/control", session));
        body.push_str(&hidden("account_id", id));
        body.push_str(&hidden("action", "policy"));
        body.push_str(&select(
            "enabled",
            "Acquisition enabled",
            if policy.enabled { "yes" } else { "no" },
            ["no".into(), "yes".into()],
        ));
        body.push_str(&select(
            "approval_required",
            "Require approval",
            if policy.approval_required {
                "yes"
            } else {
                "no"
            },
            ["yes".into(), "no".into()],
        ));
        body.push_str(&format!("<label>Maximum active requests<input type=number name=max_active min=1 max=64 value={}></label><label>Maximum requests per UTC day<input type=number name=max_daily min=1 max=1024 value={}></label>",policy.max_active,policy.max_daily));
        for (name, label, value) in [
            ("movie_profile", "Movie profile", &policy.movie_profile),
            (
                "episode_profile",
                "Episode profile",
                &policy.episode_profile,
            ),
        ] {
            body.push_str(&select(
                name,
                label,
                value,
                engine.config.selection.profiles.keys().cloned(),
            ));
        }
        body.push_str(&select(
            "destination",
            "Library destination",
            &policy.destination,
            std::iter::once("default".into()).chain(
                engine
                    .config
                    .requesters
                    .destinations
                    .iter()
                    .map(|r| r.id.clone()),
            ),
        ));
        body.push_str(&select(
            "notifications",
            "Recorded notification outcomes",
            &policy.notifications,
            ["none".into(), "decisions".into(), "all".into()],
        ));
        body.push_str("<p>Edits apply to future admissions. Existing acquisition policies and daily charges remain captured.</p><button>Review policy</button></form>");
    } else {
        body.push_str(
            "<p>This account is no longer configured. Imported files remain available.</p>",
        );
    }
    body.push_str("</section><section class=panel><h2>Requests</h2><table><thead><tr><th>Title</th><th>State</th><th>Acquisition</th><th>Decision</th></tr></thead><tbody>");
    for d in array(report.get("demands").unwrap_or(&Value::Null)) {
        let request = d.get("request").unwrap_or(&Value::Null);
        body.push_str(&format!(
            "<tr><td>{} {} S{}E{}</td><td>{}</td><td>",
            display(text(request, "title")),
            scalar(request, "year"),
            scalar(request, "season"),
            scalar(request, "episode"),
            e(text(d, "state"))
        ));
        if let Some(job) = d.get("job_id").and_then(Value::as_str) {
            body.push_str(&format!("<a href=\"/ui/jobs/{}\">View job</a>", e(job)));
        }
        body.push_str("</td><td>");
        if report.get("configured").and_then(Value::as_bool) == Some(true) {
            body.push_str(&form("/ui/requesters/control", session));
            body.push_str(&hidden("account_id", id));
            body.push_str(&hidden("demand_id", text(d, "id")));
            if d.get("job_id").and_then(Value::as_str).is_none()
                && matches!(text(d, "state"), "pending" | "quota" | "conflict")
            {
                body.push_str("<button name=action value=approve>Review approval</button><button name=action value=reject>Review rejection</button>");
            }
            if !matches!(text(d, "state"), "removed" | "rejected") {
                body.push_str("<button name=action value=remove>Review removal</button>");
            }
            if d.get("job_id").and_then(Value::as_str).is_some()
                && matches!(text(d, "outcome"), "failed" | "cancelled")
            {
                body.push_str("<button name=action value=retry>Review retry</button>");
            }
            body.push_str("</form>");
        }
        body.push_str("</td></tr>");
    }
    body.push_str("</tbody></table>");
    let total = report.get("total").and_then(Value::as_u64).unwrap_or(0) as usize;
    if offset > 0 {
        body.push_str(&format!(
            "<a href=\"/ui/requesters/{}?offset={}&amp;limit={}\">Previous requests</a> ",
            e(id),
            offset.saturating_sub(limit),
            limit
        ));
    }
    if offset + limit < total {
        body.push_str(&format!(
            "<a href=\"/ui/requesters/{}?offset={}&amp;limit={}\">Next requests</a>",
            e(id),
            offset + limit,
            limit
        ));
    }
    body.push_str("</section><section class=panel><h2>Notification outcomes</h2><p>These outcomes are recorded in Mynou. External delivery is a later integration.</p><ul>");
    for n in array(report.get("notifications").unwrap_or(&Value::Null)) {
        body.push_str(&format!(
            "<li>{}: {} at {}</li>",
            e(text(n, "demand_id")),
            e(text(n, "outcome")),
            scalar(n, "at")
        ));
    }
    body.push_str("</ul></section>");
    Ok(frame(
        "Requester details",
        "/ui/requesters",
        Some(session),
        &body,
    ))
}
pub(super) fn query(form: &Form) -> Result<ControlRequest> {
    let action = form.value("action")?;
    let mut v = Value::object();
    v.insert("action", action);
    if action == "policy" {
        form.only(&[
            "csrf",
            "account_id",
            "action",
            "enabled",
            "approval_required",
            "max_active",
            "max_daily",
            "movie_profile",
            "episode_profile",
            "destination",
            "notifications",
        ])?;
        let mut p = Value::object();
        for key in ["enabled", "approval_required"] {
            p.insert(
                key,
                match form.value(key)? {
                    "yes" => true,
                    "no" => false,
                    _ => return Err("Choose yes or no for requester policy".into()),
                },
            );
        }
        for (key, max) in [("max_active", 64), ("max_daily", 1024)] {
            p.insert(key, decimal(form.value(key)?, max, key)? as u32);
        }
        for key in [
            "movie_profile",
            "episode_profile",
            "destination",
            "notifications",
        ] {
            p.insert(key, form.value(key)?);
        }
        v.insert("policy", p);
    } else {
        form.only(&["csrf", "account_id", "action", "demand_id"])?;
        v.insert("demand_id", form.value("demand_id")?);
    }
    ControlRequest::from_json(&v)
}
pub(super) fn review(session: &Session, id: &str, report: &Value) -> String {
    let mut body = format!(
        "<section class=panel><h1>Review requester decision</h1><p>Account: {}. Action: {}.</p><p>Compatible demand may share acquisition. Quotas are checked before acquisition; removal retains other interests and imported media.</p>",
        e(id),
        e(text(report, "action"))
    );
    if let Some(p) = report.get("policy") {
        body.push_str(&format!("<p>Enabled: {}. Approval required: {}. Active limit: {}. Daily limit: {}.</p><p>Movie profile: {}. Episode profile: {}. Destination: {}. Notifications: {}.</p>",scalar(p,"enabled"),scalar(p,"approval_required"),scalar(p,"max_active"),scalar(p,"max_daily"),e(text(p,"movie_profile")),e(text(p,"episode_profile")),e(text(p,"destination")),e(text(p,"notifications"))));
    }
    if let Some(d) = report.get("demand") {
        body.push_str(&format!(
            "<p>Demand: {}. Title: {}. State: {}.</p>",
            e(text(d, "id")),
            display(text(d.get("request").unwrap_or(&Value::Null), "title")),
            e(text(d, "state"))
        ));
    }
    body.push_str(&form("/ui/requesters/control", session));
    for (key, value) in [
        ("account_id", id),
        ("action", text(report, "action")),
        ("plan_id", text(report, "plan_id")),
        ("apply", "yes"),
    ] {
        body.push_str(&hidden(key, value));
    }
    body.push_str("<button>Apply reviewed decision</button></form></section>");
    frame("Requester review", "/ui/requesters", Some(session), &body)
}
