//! Escaped alias-only native source health.
use super::{
    session::Session,
    views::{array, e, frame, scalar, text},
};
use crate::{Result, engine::Engine, json::Value};
use std::sync::Arc;
pub(super) fn list(engine: &Arc<Engine>, session: &Session) -> Result<String> {
    let report = crate::indexers::report(&engine.config.sources);
    let mut body = String::from(
        "<section class=panel><h1>Indexers</h1><p>Native source authentication and transport health. Session renewal keeps credentials on the configured origin. Interactive logins are unsupported.</p><table><tr><th>Source</th><th>Adapter</th><th>Enabled</th><th>Authentication</th><th>Requests</th><th>HTTP status</th><th>Health</th></tr>",
    );
    for s in array(report.get("sources").unwrap_or(&Value::Null)) {
        body.push_str(&format!("<tr><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td></tr>",e(text(s,"name")),e(text(s,"kind")),scalar(s,"enabled"),e(text(s,"authentication")),scalar(s,"requests"),scalar(s,"last_status"),if s.get("busy").and_then(Value::as_bool)==Some(true){"busy".into()}else {e(text(s,"last_error"))}));
    }
    body.push_str("</table></section>");
    Ok(frame("Indexers", "/ui/indexers", Some(session), &body))
}
