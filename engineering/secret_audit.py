"""CI-only redacted audit of reachable Git objects and completed Actions logs."""
import argparse
from concurrent.futures import ThreadPoolExecutor
from datetime import datetime, timezone
import io
import json
import os
from pathlib import Path
import re
import subprocess
import sys
from threading import Lock
import urllib.error
import urllib.parse
import urllib.request
import zipfile

from log_coverage import Coverage, MAX_ENTRIES, instant, moment, run_identity

MAX_OBJECT = 8 * 1024 * 1024
MAX_ARCHIVE = 64 * 1024 * 1024
MAX_EXPANDED = 128 * 1024 * 1024
MAX_API_REQUESTS = 2500
API_RESERVE = 200
# A bootstrap lists every run; routine audits stop at their window.
MAX_PAGES = 1000
# Archives downloaded per run. A larger backlog continues in the next audit.
MAX_SCANS = 1000
# Bump whenever scanning semantics change (what is read or how rules apply), so
# earlier receipts stop authorizing reuse. Editing RULES changes the policy too.
SCANNER_VERSION = 1
RULES = {
    "github-token": rb"(?<![A-Za-z0-9])(?:gh[psoru]_[A-Za-z0-9]{36,255}|github_pat_[A-Za-z0-9_]{60,255})(?![A-Za-z0-9])",
    "openai-token": rb"(?<![A-Za-z0-9])sk-(?:proj-)?[A-Za-z0-9_-]{40,255}(?![A-Za-z0-9])",
    "aws-access-key": rb"(?<![A-Z0-9])(?:AKIA|ASIA)[A-Z0-9]{16}(?![A-Z0-9])",
    "docker-token": rb"(?<![A-Za-z0-9])dckr_pat_[A-Za-z0-9_-]{20,255}(?![A-Za-z0-9])",
    "slack-token": rb"(?<![A-Za-z0-9])xox[baprs]-[A-Za-z0-9-]{20,255}(?![A-Za-z0-9])",
    "stripe-live-key": rb"(?<![A-Za-z0-9])sk_live_[A-Za-z0-9]{16,255}(?![A-Za-z0-9])",
    "private-key": rb"-----BEGIN (?:RSA |EC |DSA |OPENSSH |ENCRYPTED )?PRIVATE KEY-----",
}
PATTERNS = {name: re.compile(pattern) for name, pattern in RULES.items()}


def findings(data, location):
    # No matched bytes, previews, raw filenames or hashes of secret values.
    return [{"rule": name, "location": location}
            for name, pattern in PATTERNS.items() if pattern.search(data)]


class AuditError(Exception):
    pass


class HTTPFailure(AuditError):
    def __init__(self, status):
        self.status = status


class BudgetFailure(AuditError):
    pass


def rate_metadata(headers):
    """Only bounded numeric provider counters and a fixed resource name."""
    result = {}
    for header, key in [
            ("X-RateLimit-Limit", "limit"), ("X-RateLimit-Remaining", "remaining"),
            ("X-RateLimit-Used", "used"), ("X-RateLimit-Reset", "reset_epoch"),
            ("Retry-After", "retry_after_seconds")]:
        value = headers.get(header, "")
        if isinstance(value, str) and re.fullmatch(r"[0-9]{1,18}", value):
            result[key] = int(value)
    if headers.get("X-RateLimit-Resource") in ("core", "search", "graphql"):
        result["resource"] = headers["X-RateLimit-Resource"]
    return result


class RequestBudget:
    def __init__(self):
        self.lock = Lock()
        self.calls = 0
        self.rate = {}

    def capacity(self):
        """Requests still available before the hard ceiling or observed reserve."""
        with self.lock:
            spare = MAX_API_REQUESTS - self.calls
            remaining = self.rate.get("remaining")
            if remaining is not None:
                spare = min(spare, remaining - API_RESERVE)
            return max(0, spare)

    def require(self, minimum=1):
        with self.lock:
            remaining = self.rate.get("remaining")
            if (type(minimum) is not int or minimum < 0
                    or self.calls + minimum > MAX_API_REQUESTS
                    or (remaining is not None and remaining - minimum < API_RESERVE)):
                raise BudgetFailure("Observed request capacity is insufficient")

    def begin(self):
        # One reservation under the lock, including deterministic I/O threads.
        with self.lock:
            remaining = self.rate.get("remaining")
            if (self.calls >= MAX_API_REQUESTS
                    or (remaining is not None and remaining <= API_RESERVE)):
                raise BudgetFailure("Request budget exhausted")
            self.calls += 1
            if remaining is not None:
                self.rate["remaining"] = remaining - 1

    def observe(self, headers):
        value = rate_metadata(headers)
        with self.lock:
            if "remaining" in value and "remaining" in self.rate:
                value["remaining"] = min(value["remaining"], self.rate["remaining"])
            self.rate.update(value)

    def evidence(self):
        with self.lock:
            return {"requests": self.calls, "request_limit": MAX_API_REQUESTS,
                    "reserve": API_RESERVE, "observed_rate": dict(self.rate)}


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        return None


def archive_url(value):
    parsed = urllib.parse.urlsplit(value)
    host = parsed.hostname or ""
    if (parsed.scheme != "https" or parsed.username or parsed.password
            or parsed.port not in (None, 443) or parsed.fragment
            or not (host.endswith(".blob.core.windows.net")
                    or host.endswith(".githubusercontent.com")
                    or host == "api.github.com")):
        raise AuditError("Unsupported log redirect")
    return value


class API:
    def __init__(self, repository, token):
        if not re.fullmatch(r"[A-Za-z0-9_-]+/[A-Za-z0-9_.-]+", repository):
            raise AuditError("Invalid repository")
        self.repository = repository
        self.base = "https://api.github.com/repos/" + repository
        self.token = token
        self.client = urllib.request.build_opener(NoRedirect())
        self.budget = RequestBudget()

    def get(self, path, archive=False, limit=None):
        self.budget.begin()
        headers = {"Accept": "application/vnd.github+json",
                   "Cache-Control": "no-cache", "User-Agent": "mynou-secret-audit",
                   "X-GitHub-Api-Version": "2022-11-28",
                   "Authorization": "Bearer " + self.token}
        request = urllib.request.Request(self.base + path, headers=headers)
        limit = limit if limit is not None else (MAX_ARCHIVE if archive else 8 * 1024 * 1024)
        if type(limit) is not int or not 0 < limit <= MAX_ARCHIVE:
            raise AuditError("Invalid response bound")
        try:
            with self.client.open(request, timeout=30) as response:
                self.budget.observe(response.headers)
                data = response.read(limit + 1)
        except urllib.error.HTTPError as error:
            self.budget.observe(error.headers)
            if not archive or error.code != 302:
                raise HTTPFailure(error.code) from None
            # A signed storage request receives no GitHub Authorization header.
            destination = archive_url(error.headers.get("Location", ""))
            request = urllib.request.Request(destination)
            try:
                with self.client.open(request, timeout=30) as response:
                    data = response.read(limit + 1)
            except urllib.error.HTTPError as nested:
                raise HTTPFailure(nested.code) from None
        if len(data) > limit:
            raise AuditError("Response exceeds audit bound")
        return data if archive else json.loads(data)

    def pages(self, path, key):
        separator = "&" if "?" in path else "?"
        for page in range(1, MAX_PAGES + 1):
            value = self.get(path + separator + "per_page=100&page=" + str(page))
            rows = value[key]
            yield from rows
            if len(rows) < 100:
                return
        raise AuditError("Pagination exceeds audit bound")


def git_history():
    result = subprocess.run(["git", "rev-list", "--objects", "--all", "--no-object-names"],
                            stdout=subprocess.PIPE, stderr=subprocess.DEVNULL,
                            check=True, timeout=120)
    objects = list(dict.fromkeys(result.stdout.splitlines()))
    if len(objects) > 300000 or any(not re.fullmatch(rb"[0-9a-f]{40}", s) for s in objects):
        raise AuditError("Invalid or oversized Git object inventory")
    found, count = [], 0
    process = subprocess.Popen(["git", "cat-file", "--batch"],
                               stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                               stderr=subprocess.DEVNULL)
    try:
        for oid in objects:
            process.stdin.write(oid + b"\n")
            process.stdin.flush()
            header = process.stdout.readline(256).split()
            if len(header) != 3 or header[0] != oid:
                raise AuditError("Invalid Git object response")
            size = int(header[2])
            if not 0 <= size <= MAX_OBJECT:
                raise AuditError("Git object exceeds audit bound")
            data = process.stdout.read(size)
            if len(data) != size or process.stdout.read(1) != b"\n":
                raise AuditError("Incomplete Git object")
            count += 1
            found.extend(findings(data, "git-object:" + oid.decode()))
        process.stdin.close()
        if process.wait(timeout=10) != 0:
            raise AuditError("Git object reader failed")
    finally:
        if process.poll() is None:
            process.kill()
            process.wait()
    return {"scope": "all reachable fetched refs and their Git objects",
            "objects_scanned": count, "findings": found[:100],
            "finding_count": len(found), "coverage_gaps": []}


def scan_archive(data, location):
    found, expanded, members = [], 0, 0
    with zipfile.ZipFile(io.BytesIO(data)) as archive:
        inventory = archive.infolist()
        if len(inventory) > 10000:
            raise AuditError("Too many log archive members")
        for info in inventory:
            if info.is_dir():
                continue
            expanded += info.file_size
            if info.flag_bits & 1 or info.file_size > MAX_ARCHIVE or expanded > MAX_EXPANDED:
                raise AuditError("Log archive exceeds audit bound")
            with archive.open(info) as stream:
                payload = stream.read(MAX_ARCHIVE + 1)
            if len(payload) != info.file_size or len(payload) > MAX_ARCHIVE:
                raise AuditError("Incomplete or oversized log member")
            found.extend(findings(payload, location + "/member:" + str(members)))
            members += 1
    return members, found


def audit_attempt(api, item):
    run, attempt = item
    identity = "actions-run:" + str(run) + "/attempt:" + str(attempt)
    path = "/actions/runs/" + str(run) + "/attempts/" + str(attempt)
    try:
        data = api.get(path + "/logs", archive=True)
    except HTTPFailure as error:
        if error.status not in (404, 410):
            raise
        jobs = list(api.pages(path + "/jobs", "jobs"))
        if not any(j.get("runner_id") or j.get("steps") for j in jobs):
            return {"state": "no-runner-execution", "location": identity}
        if error.status == 410:
            # Retention deleted these logs: nothing remains to expose or scan.
            return {"state": "expired", "location": identity}
        return {"state": "unavailable", "location": identity, "http": error.status}
    members, found = scan_archive(data, identity)
    return {"state": "scanned", "location": identity, "members": members, "findings": found}


def inventory(api, since):
    """Newest-first runs created at or after since; every visible run without it."""
    runs, seen, older = [], set(), 0
    for run in api.pages("/actions/runs", "workflow_runs"):
        run_id = run.get("id")
        if type(run_id) is not int or not 0 < run_id < 2 ** 63:
            raise AuditError("Invalid run inventory")
        if since is not None and moment(run.get("created_at")) < since:
            older += 1
            # Runs arrive newest first: a full page of older runs ends the window.
            if older >= 100:
                break
            continue
        older = 0
        if run_id in seen:
            # Runs created during listing shift later pages, so a page can repeat
            # a run that an earlier page returned. Keep the earlier observation.
            continue
        seen.add(run_id)
        runs.append(run)
    return runs


def actions_logs(api, coverage=None, now=None):
    now = now or datetime.now(timezone.utc)
    if coverage is not None:
        coverage.load()
    since, deep = coverage.scope(now) if coverage is not None else (None, True)
    runs = inventory(api, since)
    attempts, pending, bindings, idle = [], [], {}, set()
    for run in runs:
        run_id = run["id"]
        number = run.get("run_attempt", 1)
        if type(number) is not int or not 1 <= number <= 100:
            raise AuditError("Invalid attempt inventory")
        if coverage is not None:
            bindings[run_id] = run_identity(run)
        completed = number
        if run["status"] != "completed":
            pending.append("actions-run:" + str(run_id))
            completed -= 1
        elif run.get("conclusion") == "skipped":
            # Every job of the latest attempt was skipped: no runner wrote a log.
            idle.add((run_id, number))
        if len(attempts) + completed > MAX_ENTRIES:
            raise AuditError("Attempt inventory exceeds receipt bound")
        attempts.extend((run_id, a) for a in range(1, completed + 1))
    reused, uncovered, skipped = [], [], []
    for item in attempts:
        row = coverage.covered(bindings[item[0]], item[1]) if coverage is not None else None
        if row is not None:
            reused.append(row)
        elif item in idle:
            skipped.append(item)
        else:
            uncovered.append(item)
    # Newest attempts first. A bounded batch leaves the rest to the next audit, so
    # no backlog can exceed one run's request ceiling or the observed reserve.
    uncovered.sort(reverse=True)
    batch = min(len(uncovered), MAX_SCANS, api.budget.capacity() // 2)
    if uncovered and not batch:
        raise BudgetFailure("Observed request capacity is insufficient")
    selected, deferred = uncovered[:batch], uncovered[batch:]
    api.budget.require(len(selected))
    found, unavailable, entries = [], [], list(reused)
    counts = {"scanned": 0, "no-runner-execution": len(skipped), "expired": 0}
    members = 0
    if coverage is not None:
        entries.extend({**bindings[run_id], "attempt": attempt,
                        "state": "no-runner-execution", "members": 0}
                       for run_id, attempt in skipped)
    # Deterministic CI I/O, never additional AI workers. Only four submissions
    # exist at once; a failure cannot drain a large queued archive inventory.
    with ThreadPoolExecutor(max_workers=4) as pool:
        for start in range(0, len(selected), 4):
            results = list(pool.map(lambda item: audit_attempt(api, item),
                                    selected[start:start + 4]))
            for item, result in zip(selected[start:start + 4], results):
                if result["state"] == "unavailable":
                    unavailable.append(result["location"])
                    continue
                counts[result["state"]] += 1
                members += result.get("members", 0)
                found.extend(result.get("findings", []))
                if coverage is not None:
                    run_id, attempt = item
                    entries.append({**bindings[run_id], "attempt": attempt,
                                    "state": result["state"],
                                    "members": result.get("members", 0)})
    report = {"scope": "completed visible workflow attempts in the audited window",
              "inventory_at": instant(now),
              "since": None if since is None else instant(since), "deep": deep,
              "runs_enumerated": len(runs), "attempts_examined": len(attempts),
              "archives_scanned": counts["scanned"], "attempts_reused": len(reused),
              "attempts_without_execution": counts["no-runner-execution"],
              "attempts_expired": counts["expired"], "attempts_deferred": len(deferred),
              "members_scanned": members, "pending_runs": pending,
              "coverage_gaps": unavailable, "findings": found[:100],
              "finding_count": len(found)}
    if coverage is not None:
        carried = coverage.carried({run["id"] for run in runs}, deep)
        entries.extend(carried)
        entries.sort(key=lambda row: (row["run_id"], row["attempt"]))
        report["attempts_carried"] = len(carried)
        report["attempts_recorded"] = len(entries)
        report["coverage"] = coverage.receipt(entries, now, since, deep,
                                              not deferred and not unavailable)
        report["baseline"] = coverage.baseline
        report["full_sweep"] = coverage.full
    return report


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("scope", choices=["history", "logs"])
    parser.add_argument("--output", required=True)
    parser.add_argument("--full", action="store_true")
    args = parser.parse_args()
    api = None
    try:
        if args.scope == "history":
            report = git_history()
        else:
            token = os.environ.get("GH_TOKEN", "")
            if not token:
                raise AuditError("Missing Actions read credential")
            api = API(os.environ.get("GITHUB_REPOSITORY", ""), token)
            full = args.full or os.environ.get("MYNOU_FULL_LOG_AUDIT") == "true"
            report = actions_logs(api, Coverage(api, full=full))
            report["api_budget"] = api.budget.evidence()
        report["schema"] = 3 if args.scope == "logs" else 1
        report["status"] = ("findings" if report["finding_count"] else
                            "incomplete" if report["coverage_gaps"] else
                            "partial" if report.get("attempts_deferred") else "passed")
        # Compact JSON keeps a month of attempt receipts far below the bound.
        Path(args.output).write_text(json.dumps(report, separators=(",", ":")) + "\n")
        print(json.dumps({key: value for key, value in report.items() if key != "coverage"}))
        if report["status"] == "partial":
            print("::notice title=Actions log audit::" + str(report["attempts_deferred"])
                  + " uncovered attempts continue in the next audit")
        return 0 if report["status"] in ("passed", "partial") else 1
    except Exception as error:
        # Do not print exception messages/URLs/bodies or raw downloaded contents.
        report = {"schema": 1, "status": "error", "error_type": type(error).__name__}
        if isinstance(error, HTTPFailure):
            report["http_status"] = error.status
        if api is not None:
            report["api_budget"] = api.budget.evidence()
        Path(args.output).write_text(json.dumps(report) + "\n")
        print(json.dumps(report))
        return 1


if __name__ == "__main__":
    sys.exit(main())
