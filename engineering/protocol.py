"""Comment commands that coordinate agents on Issues and PRs; pure functions.

A command is the first line of a comment written by a trusted account. Every
other comment, and every untrusted comment, is ordinary data."""
from datetime import datetime, timedelta, timezone
import re

AGENT = r"[a-z0-9][a-z0-9-]{1,39}"
WAIT = ("author", "qa", "security", "user", "ci")
PATTERNS = (
    (re.compile(r"/(assign|unassign) (" + AGENT + r")"), lambda m: (m[1], m[2])),
    (re.compile(r"/wait (" + "|".join(WAIT) + r")"), lambda m: ("wait", m[1])),
    (re.compile(r"/(approve|reject) (qa|security) ([0-9a-f]{40})"), lambda m: (m[1], m[2], m[3])),
)
SENSITIVE = ("AGENTS.md", "engineering/", ".agents/", ".github/", "src/crypto", "src/tls", "src/pki")
CLOSING = re.compile(r"\b(?:close[sd]?|fix(?:e[sd])?|resolve[sd]?)\s+#([1-9][0-9]*)\b", re.I)
CLEAN = ("success", "skipped", "neutral")
# Some connectors defang slash commands with middle dots or zero-width
# characters (for example "·/·a·pprove"); from a trusted account they keep
# their meaning.
DEFANG = dict.fromkeys(map(ord, "\u00b7\u200b\u200c\u200d\u2060\ufeff"))


def instant(value):
    """Parse a GitHub UTC timestamp such as 2026-10-09T15:20:44Z."""
    if not isinstance(value, str):
        raise ValueError("GitHub timestamp is missing")
    return datetime.strptime(value, "%Y-%m-%dT%H:%M:%SZ").replace(tzinfo=timezone.utc)


def stamp(value):
    return value.astimezone(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


def labels(item):
    return {label["name"] for label in item.get("labels", [])}


def parse(body):
    """Return the command on a comment's first line, or None for ordinary text."""
    if not isinstance(body, str):
        return None
    text = body.translate(DEFANG).strip()
    if not text:
        return None
    first = text.splitlines()[0].strip()
    for pattern, build in PATTERNS:
        match = pattern.fullmatch(first)
        if match:
            return build(match)
    return None


def commands(comments, actors):
    """Yield (comment, command) for trusted commands in creation order."""
    for comment in sorted(comments, key=lambda c: (instant(c["created_at"]), c["id"])):
        user = comment.get("user") or {}
        login = user.get("login")
        if login not in actors or user.get("id") != actors[login]:
            continue
        command = parse(comment.get("body"))
        if command is not None:
            yield comment, command


def claim(comments, actors):
    """The owner of an Issue or PR: the first /assign made while nobody owns it."""
    owner = None
    for comment, command in commands(comments, actors):
        if command[0] == "assign" and owner is None:
            owner = {"agent": command[1], "since": comment["created_at"]}
        elif command[0] == "unassign" and owner is not None and owner["agent"] == command[1]:
            owner = None
    return owner


def lapsed(activity, now, ttl_minutes):
    """A claim lapses when its Issue and PR show no activity for the TTL."""
    return now - max(instant(value) for value in activity) > timedelta(minutes=ttl_minutes)


def verdicts(comments, actors, head):
    """The latest QA and Security verdicts that name this exact head commit."""
    found = {}
    for _, command in commands(comments, actors):
        if command[0] in ("approve", "reject") and command[2] == head:
            found[command[1]] = command[0]
    return found


def turn(comments, actors, head, draft, security_required, ci=None):
    """Who acts next on a PR; `merge` once every required verdict approves the head.

    `/wait ci` settles once CI completes: a failure or a draft goes back to the
    author, a ready PR with green checks to QA."""
    found = verdicts(comments, actors, head)
    if found.get("qa") == "approve" and (not security_required or found.get("security") == "approve"):
        return "merge"
    who = "author" if draft else "qa"
    for _, command in commands(comments, actors):
        if command[0] == "wait":
            who = command[1]
        elif command[0] == "reject" and command[2] == head:
            who = "author"
    if who == "ci" and ci in ("success", "failure"):
        who = "qa" if ci == "success" and not draft else "author"
    return who


def security_required(paths, issue_labels):
    return (any(path.startswith(SENSITIVE) for path in paths)
            or bool(set(issue_labels) & {"risk:high", "risk:critical"}))


def ci_state(check_runs):
    """`success` only when every latest check run on the head completed cleanly."""
    if not check_runs:
        return "missing"
    if any(run.get("status") != "completed" for run in check_runs):
        return "pending"
    if all(run.get("conclusion") in CLEAN for run in check_runs):
        return "success"
    return "failure"


def linked_issues(body):
    return sorted({int(number) for number in CLOSING.findall(body or "")})


def rank(issue):
    """Priority order: p0 first, the owner's requests first, then oldest."""
    names = labels(issue)
    priority = next((n for n in range(4) if f"priority:p{n}" in names), 2)
    return priority, "origin:user" not in names, issue["number"]


def blocked(issue):
    return ("status:blocked" in labels(issue)
            or (issue.get("issue_dependencies_summary") or {}).get("blocked_by", 0) > 0)


def ready_proposals(issues, planning_issue):
    """Unblocked Ready product increments that agents proposed."""
    return sum(
        issue["number"] != planning_issue
        and "status:ready" in labels(issue)
        and bool(labels(issue) & {"origin:agent", "origin:ux", "origin:reliability"})
        and bool(labels(issue) & {"enhancement", "bug", "accessibility"})
        and (issue.get("issue_dependencies_summary") or {}).get("blocked_by") == 0
        for issue in issues)


def planning_due(issues, policy, now):
    """Product reviews the backlog when it runs low, at most once per interval."""
    anchor = next((issue for issue in issues if issue["number"] == policy["issue"]), None)
    if anchor is None or ready_proposals(issues, policy["issue"]) >= policy["ready_minimum"]:
        return False
    return now - instant(anchor["updated_at"]) >= timedelta(hours=policy["review_interval_hours"])
