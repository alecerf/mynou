//! Provider aliases and reviewed non-acquiring connection probes.
use super::{
    session::Session,
    views::{array, e, form, frame, hidden, scalar, text},
};
use crate::{engine::Engine, json::Value};
use std::sync::Arc;
pub(super) fn list(engine: &Arc<Engine>, session: &Session) -> String {
    let report = engine.usenet_servers();
    let mut body = String::from(
        "<section class=panel><h1>Usenet</h1><p>Review a connection probe to check greeting and authentication. Probes never request articles or queue media.</p><table><tr><th>Provider</th><th>TLS</th><th>Authentication</th><th>Attempts</th><th>Health</th><th>Probe</th></tr>",
    );
    for s in array(report.get("servers").unwrap_or(&Value::Null)) {
        body.push_str(&format!(
            "<tr><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>",
            e(text(s, "id")),
            scalar(s, "tls"),
            scalar(s, "authenticated"),
            scalar(s, "attempts"),
            if s.get("busy").and_then(Value::as_bool) == Some(true) {
                "busy".into()
            } else {
                e(text(s, "last_error"))
            }
        ));
        body.push_str(&form("/ui/usenet/probe", session));
        body.push_str(&hidden("id", text(s, "id")));
        body.push_str("<button>Review connection probe</button></form></td></tr>");
    }
    body.push_str("</table></section>");
    frame("Usenet", "/ui/usenet", Some(session), &body)
}
pub(super) fn review(session: &Session, report: &Value) -> String {
    let mut body = format!(
        "<section class=panel><h1>Review Usenet probe</h1><p>Provider: {}. This checks one connection and authentication without reading an article. Review expires when provider settings, service identity or attempt count change.</p>",
        e(text(report, "id"))
    );
    body.push_str(&form("/ui/usenet/probe", session));
    for (k, v) in [
        ("id", text(report, "id")),
        ("plan_id", text(report, "plan_id")),
        ("apply", "yes"),
    ] {
        body.push_str(&hidden(k, v));
    }
    body.push_str("<button>Apply reviewed connection probe</button></form></section>");
    frame("Review Usenet probe", "/ui/usenet", Some(session), &body)
}
