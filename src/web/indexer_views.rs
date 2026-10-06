//! Escaped alias-only native source health and reviewed policy controls.
use super::{
    session::Session,
    views::{array, e, form, frame, hidden, scalar, text},
};
use crate::{Result, engine::Engine, json::Value};
use std::sync::Arc;
pub(super) fn list(engine: &Arc<Engine>, session: &Session) -> Result<String> {
    let report = engine.indexers()?;
    let mut body = String::from(
        "<section class=panel><h1>Indexers</h1><p>Native source authentication and transport health. Review a control before applying it. Probes search the configured source without queuing media.</p><table><tr><th>Source</th><th>Adapter</th><th>Enabled</th><th>Authentication</th><th>Requests</th><th>HTTP status</th><th>Health</th><th>Controls</th></tr>",
    );
    for s in array(report.get("sources").unwrap_or(&Value::Null)) {
        body.push_str(&format!(
            "<tr><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>",
            e(text(s, "name")),
            e(text(s, "kind")),
            scalar(s, "enabled"),
            e(text(s, "authentication")),
            scalar(s, "requests"),
            scalar(s, "last_status"),
            if s.get("busy").and_then(Value::as_bool) == Some(true) {
                "busy".into()
            } else {
                e(text(s, "last_error"))
            }
        ));
        body.push_str(&form("/ui/indexers/control", session));
        body.push_str(&hidden("id", text(s, "id")));
        if s.get("enabled").and_then(Value::as_bool) == Some(true) {
            body.push_str("<button name=action value=pause>Review pause</button><button name=action value=probe>Review probe</button>");
        } else {
            body.push_str("<button name=action value=enable>Review enable</button>");
        }
        body.push_str("<button name=action value=reset_session>Review session reset</button></form></td></tr>");
    }
    body.push_str("</table></section>");
    Ok(frame("Indexers", "/ui/indexers", Some(session), &body))
}
pub(super) fn review(session: &Session, report: &Value) -> String {
    let mut body = format!(
        "<section class=panel><h1>Review source {}</h1><p>Source: {}. Changes are saved before use. A probe makes a bounded source search and never queues a download.</p>",
        e(text(report, "action")),
        e(text(report, "name"))
    );
    body.push_str(&form("/ui/indexers/control", session));
    for (k, v) in [
        ("id", text(report, "id")),
        ("action", text(report, "action")),
        ("plan_id", text(report, "plan_id")),
        ("apply", "yes"),
    ] {
        body.push_str(&hidden(k, v));
    }
    body.push_str("<button>Apply reviewed source control</button></form></section>");
    frame("Review source", "/ui/indexers", Some(session), &body)
}
