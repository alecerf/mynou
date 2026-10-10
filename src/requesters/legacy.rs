//! Checked, idempotent migration of a `requesters.bin` payload written before
//! manual approvals and quotas were removed. The removed fields are validated and
//! then dropped; they never grant authority. Demand that was not approved
//! (awaiting approval or quota-blocked) becomes `held` and needs an explicit
//! operator `admit`; rejected demand becomes a removal tombstone.
use super::{MAX_DEMANDS, Result, Value};

const POLICY_FIELDS: [&str; 3] = ["approval_required", "max_active", "max_daily"];

fn count(v: &Value, keys: &[&str]) -> usize {
    keys.iter().filter(|k| v.get(k).is_some()).count()
}
fn number(v: &Value, key: &str) -> Option<u64> {
    v.get(key)
        .and_then(|v| v.as_u64().or_else(|| v.as_str()?.parse().ok()))
}
fn policy_is_legacy(policy: &Value) -> Result<bool> {
    match count(policy, &POLICY_FIELDS) {
        0 => Ok(false),
        3 => Ok(true),
        _ => Err("Requester: partial legacy policy fields".into()),
    }
}
fn demand_is_legacy(demand: &Value) -> Result<bool> {
    match (
        count(demand, &["approved", "charged_at"]),
        demand.get("admitted_at").is_some(),
    ) {
        (0, _) => Ok(false),
        (2, false) => Ok(true),
        _ => Err("Requester: mixed legacy and current demand fields".into()),
    }
}
/// True when the payload still carries pre-removal fields.
pub(super) fn is_legacy(state: &Value) -> bool {
    let any = |key: &str, test: fn(&Value) -> Result<bool>| {
        state
            .get(key)
            .and_then(Value::as_array)
            .is_some_and(|items| items.iter().any(|i| test(i).unwrap_or(true)))
    };
    state
        .get("accounts")
        .and_then(Value::as_array)
        .is_some_and(|a| {
            a.iter().any(|r| {
                r.get("policy")
                    .is_some_and(|p| policy_is_legacy(p).unwrap_or(true))
            })
        })
        || any("demands", demand_is_legacy)
}
fn migrate_policy(policy: &Value) -> Result<Value> {
    if !policy_is_legacy(policy)? {
        return Ok(policy.clone());
    }
    let (active, daily) = (number(policy, "max_active"), number(policy, "max_daily"));
    if policy
        .get("approval_required")
        .and_then(Value::as_bool)
        .is_none()
        || !active.is_some_and(|n| (1..=64).contains(&n))
        || !daily.is_some_and(|n| (1..=1024).contains(&n))
    {
        return Err("Requester: invalid legacy policy".into());
    }
    let mut out = policy.clone();
    if let Value::Object(m) = &mut out {
        for key in POLICY_FIELDS {
            m.remove(key);
        }
    }
    Ok(out)
}
fn held_outcome(outcome: &str) -> &str {
    match outcome {
        "quota" => "held",
        "rejected" => "removed",
        other => other,
    }
}
fn migrate_demand(demand: &Value) -> Result<Value> {
    if !demand_is_legacy(demand)? {
        return Ok(demand.clone());
    }
    let approved = demand
        .get("approved")
        .and_then(Value::as_bool)
        .ok_or("Requester: invalid approved")?;
    let charged = demand.get("charged_at").cloned().unwrap_or(Value::Null);
    if !matches!(charged, Value::Null) && number(demand, "charged_at").is_none() {
        return Err("Requester: invalid charged_at".into());
    }
    let unadmitted =
        matches!(charged, Value::Null) && matches!(demand.get("job_id"), None | Some(Value::Null));
    let state = demand
        .get("state")
        .and_then(Value::as_str)
        .ok_or("Requester: invalid state")?;
    let outcome = demand
        .get("outcome")
        .and_then(Value::as_str)
        .ok_or("Requester: invalid outcome")?;
    let (state, outcome) = match state {
        "removed" => ("removed", held_outcome(outcome)),
        "rejected" => ("removed", "removed"),
        "quota" if unadmitted => ("held", "held"),
        "pending" | "conflict" if !approved && unadmitted => ("held", "held"),
        "pending" | "conflict" if approved => (state, held_outcome(outcome)),
        "reserved" | "active" | "ready" if approved => (state, held_outcome(outcome)),
        _ => return Err("Requester: inconsistent legacy demand".into()),
    };
    let mut out = demand.clone();
    if let Value::Object(m) = &mut out {
        m.remove("approved");
        m.remove("charged_at");
        m.insert("admitted_at".into(), charged);
        m.insert("state".into(), state.into());
        m.insert("outcome".into(), outcome.into());
    }
    Ok(out)
}
/// Returns the current-format payload; current payloads pass through unchanged.
pub(super) fn migrate(state: &Value) -> Result<Value> {
    if !is_legacy(state) {
        return Ok(state.clone());
    }
    let mut out = state.clone();
    let Value::Object(m) = &mut out else {
        return Err("Requester: invalid snapshot".into());
    };
    if let Some(Value::Array(accounts)) = m.get_mut("accounts") {
        for record in accounts.iter_mut() {
            if let Some(policy) = record.get("policy") {
                let policy = migrate_policy(policy)?;
                record.insert("policy", policy);
            }
        }
    }
    if let Some(Value::Array(demands)) = m.get_mut("demands") {
        if demands.len() > MAX_DEMANDS {
            return Err("Requester: demand capacity reached".into());
        }
        for demand in demands.iter_mut() {
            *demand = migrate_demand(demand)?;
        }
    }
    Ok(out)
}
