//! Reviewed connection probes. No article download API or implicit queue admission.
use crate::{Result, engine::Engine, json::Value};
use std::sync::atomic::Ordering;
#[derive(Clone, Debug)]
pub struct ProbeRequest {
    pub apply: bool,
    pub plan_id: Option<String>,
}
impl ProbeRequest {
    pub fn from_json(v: &Value) -> Result<Self> {
        crate::numbering::only(v, &["apply", "plan_id"])?;
        let apply = match v.get("apply") {
            None => false,
            Some(Value::Bool(b)) => *b,
            _ => return Err("Usenet: invalid apply flag".into()),
        };
        let plan_id = match v.get("plan_id") {
            None | Some(Value::Null) => None,
            Some(Value::String(s)) => Some(s.clone()),
            _ => return Err("Usenet: invalid probe guard".into()),
        };
        let q = Self { apply, plan_id };
        q.validate()?;
        Ok(q)
    }
    pub fn validate(&self) -> Result<()> {
        if (self.apply && self.plan_id.is_none())
            || self
                .plan_id
                .as_ref()
                .is_some_and(|s| !crate::requesters::valid_digest(s))
        {
            return Err("Usenet: invalid guarded probe".into());
        }
        Ok(())
    }
    pub fn to_json(&self) -> Value {
        let mut v = Value::object();
        v.insert("apply", self.apply);
        v.insert(
            "plan_id",
            self.plan_id.clone().map_or(Value::Null, Value::from),
        );
        v
    }
}
impl Engine {
    pub fn usenet_servers(&self) -> Value {
        self.config.usenet.report()
    }
    pub fn usenet_probe(&self, id: &str, q: &ProbeRequest) -> Result<Value> {
        q.validate()?;
        if !crate::requesters::valid_id(id) {
            return Err("Usenet: invalid server ID".into());
        }
        let s = self
            .config
            .usenet
            .servers
            .iter()
            .find(|s| s.id() == id)
            .ok_or("Usenet: server not configured")?;
        let guard = s.preview_guard()?;
        let mut v = Value::object();
        v.insert("id", id);
        v.insert("plan_id", guard);
        v.insert("applied", false);
        if q.apply {
            if self.read_only || self.stopped.load(Ordering::Acquire) {
                return Err("Usenet: application requires the running service".into());
            }
            let success = super::nntp::probe_guarded(s, q.plan_id.as_deref());
            // Stale/busy reviews perform no request and remain errors. Transport
            // failures consume the reviewed attempt, with fixed public diagnostics.
            match success {
                Ok(()) => {
                    v.insert("probe_success", true);
                }
                Err(e)
                    if matches!(
                        e.as_str(),
                        "NNTP: server is busy"
                            | "NNTP: probe review is stale; preview again"
                            | "NNTP: attempt counter exhausted"
                    ) =>
                {
                    return Err(e);
                }
                Err(_) => {
                    v.insert("probe_success", false);
                }
            }
            v.insert("applied", true);
        }
        Ok(v)
    }
}
