"""Pure serialized lease transitions; only the canonical control reference stores live state."""
from copy import deepcopy
from datetime import datetime, timedelta, timezone
import re

TTL = timedelta(minutes=45)
SHA = re.compile(r"[0-9a-f]{40}")


def now():
    return datetime.now(timezone.utc)


def stamp(value):
    return value.astimezone(timezone.utc).isoformat(timespec="seconds").replace("+00:00", "Z")


def time(value):
    if not isinstance(value, str) or not re.fullmatch(r"\d{4}-\d\d-\d\dT\d\d:\d\d:\d\dZ", value):
        raise ValueError("A UTC timestamp is required")
    return datetime.fromisoformat(value.replace("Z", "+00:00"))


def branch(value):
    if not isinstance(value, str) or not value or len(value) > 160 or any(
        x in value for x in ["..", "@{", "//", "\\", " ", "~", "^", ":", "?", "*", "[", "\n", "\r"]
    ) or value.startswith(("/", ".")) or value.endswith(("/", ".", ".lock")):
        raise ValueError("Invalid execution branch")
    return value


def validate(state):
    if not isinstance(state, dict) or set(state) != {"schema", "max_active_agents", "generation", "lease", "checkpoint", "attempts"}:
        raise ValueError("Unexpected control state fields; preserve rather than overwrite")
    if state["schema"] != 1 or state["max_active_agents"] != 1 or type(state["generation"]) is not int or not 0 <= state["generation"] < 1_000_000_000:
        raise ValueError("Invalid state schema, concurrency or generation")
    if not isinstance(state["checkpoint"], dict) or not isinstance(state["attempts"], dict) or len(state["attempts"]) > 256:
        raise ValueError("Invalid or oversized checkpoint/attempt data")
    held = state["lease"]
    if held is not None:
        if not isinstance(held, dict) or set(held) != {"id", "worker", "role", "issue", "acquired_at", "heartbeat_at", "expires_at", "branch", "commit", "pr"}:
            raise ValueError("Invalid lease fields")
        for key in ["id", "worker", "role"]:
            if not isinstance(held[key], str) or not 1 <= len(held[key]) <= 160:
                raise ValueError("Invalid lease identity or role")
        if type(held["issue"]) is not int or held["issue"] < 1 or (held["pr"] is not None and (type(held["pr"]) is not int or held["pr"] < 1)):
            raise ValueError("Invalid lease Issue or PR")
        branch(held["branch"])
        if not isinstance(held["commit"], str) or not SHA.fullmatch(held["commit"]):
            raise ValueError("Lease requires an exact durable commit")
        acquired, heartbeat, expires = (time(held[k]) for k in ["acquired_at", "heartbeat_at", "expires_at"])
        if acquired > heartbeat or not timedelta(0) < expires - heartbeat <= TTL:
            raise ValueError("Invalid lease lifetime")
    return state


def valid(state, at):
    validate(state)
    return state["lease"] is not None and time(state["lease"]["expires_at"]) > at


def owned(state, identity, at):
    if not valid(state, at) or state["lease"]["id"] != identity:
        raise ValueError("Lease is missing, expired or belongs to another worker; stop work")
    return state["lease"]


def acquire(state, at, identity, worker, role, issue, execution_branch, commit, pr=None):
    validate(state)
    if state["lease"] is not None:
        raise ValueError("Existing lease requires release or explicit stale-work recovery")
    result = deepcopy(state)
    result["generation"] += 1
    result["lease"] = {"id": identity, "worker": worker, "role": role, "issue": issue,
        "acquired_at": stamp(at), "heartbeat_at": stamp(at), "expires_at": stamp(at + TTL),
        "branch": execution_branch, "commit": commit, "pr": pr}
    return validate(result)


def checkpoint(state, at, identity, summary, next_action, commit=None, pr=None):
    held = owned(state, identity, at)
    if not isinstance(summary, str) or not 1 <= len(summary) <= 2000 or not isinstance(next_action, str) or not 1 <= len(next_action) <= 2000:
        raise ValueError("A concise checkpoint and next action are required")
    result = deepcopy(state)
    lease = result["lease"]
    if commit is not None:
        lease["commit"] = commit
    if pr is not None:
        lease["pr"] = pr
    lease["heartbeat_at"] = stamp(at)
    lease["expires_at"] = stamp(at + TTL)
    result["generation"] += 1
    result["checkpoint"] = {"at": stamp(at), "issue": held["issue"], "role": held["role"],
        "branch": lease["branch"], "commit": lease["commit"], "pr": lease["pr"],
        "summary": summary, "next_action": next_action}
    return validate(result)


def release(state, at, identity):
    owned(state, identity, at)
    result = deepcopy(state)
    result["generation"] += 1
    result["lease"] = None
    return validate(result)


def interruption(state, held, at, next_action):
    """Bind an interrupted phase even if it died before its first checkpoint."""
    old = state["checkpoint"]
    keys = ["issue", "role", "branch", "commit", "pr"]
    same = all(old.get(key) == held[key] for key in keys) and old.get("at") is not None and time(old["at"]) >= time(held["acquired_at"])
    result = dict(old) if same else dict((key, held[key]) for key in keys)
    if not same:
        result["summary"] = "Interrupted before a phase checkpoint; inspect the preserved lease source, Issue, PR and native history."
        if old:
            result["previous_checkpoint"] = {key: old[key] for key in ["at", *keys, "summary", "next_action"] if key in old}
    result.update(at=stamp(at), next_action=next_action)
    return result


def recover(state, at, evidence):
    validate(state)
    if state["lease"] is None or valid(state, at):
        raise ValueError("Only an expired lease can be recovered")
    if not isinstance(evidence, dict) or not all(evidence.get(k) is True for k in ["issue", "branch", "pr", "ci", "commit_preserved"]):
        raise ValueError("Stale recovery requires native Issue, branch, PR, CI and preservation evidence")
    result = deepcopy(state)
    result["generation"] += 1
    result["checkpoint"] = interruption(state, state["lease"], at,
        "Recovered expired capacity lease; inspect preserved work before selecting new work.")
    result["checkpoint"]["recovery"] = evidence
    result["lease"] = None
    return validate(result)


def attempt(state, at, identity, approach, fingerprint):
    held = owned(state, identity, at)
    if not all(isinstance(s, str) and 1 <= len(s) <= 256 for s in [approach, fingerprint]):
        raise ValueError("Attempt requires bounded approach and observed progress fingerprint")
    result = deepcopy(state)
    key = str(held["issue"])
    old = result["attempts"].get(key, {})
    repeats = old.get("unchanged", 0) + 1 if old.get("approach") == approach and old.get("fingerprint") == fingerprint else 1
    result["attempts"][key] = {"approach": approach, "fingerprint": fingerprint, "unchanged": min(repeats, 3), "at": stamp(at)}
    result["generation"] += 1
    if repeats >= 3:
        result["lease"] = None
        result["checkpoint"] = interruption(state, held, at,
            "Circuit breaker: return to Triage, inspect evidence and choose a changed approach. Do not retry unchanged work.")
        result["checkpoint"]["circuit_breaker"] = result["attempts"][key]
    return validate(result)
