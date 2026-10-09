"""CI-only redacted audit of reachable Git objects and completed Actions logs."""
import argparse
from concurrent.futures import ThreadPoolExecutor
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

MAX_OBJECT = 8 * 1024 * 1024
MAX_ARCHIVE = 64 * 1024 * 1024
MAX_EXPANDED = 128 * 1024 * 1024
MAX_API_REQUESTS = 2500
API_RESERVE = 200
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
        self.base = "https://api.github.com/repos/" + repository
        self.token = token
        self.client = urllib.request.build_opener(NoRedirect())
        self.budget = RequestBudget()

    def get(self, path, archive=False):
        self.budget.begin()
        headers = {"Accept": "application/vnd.github+json",
                   "Cache-Control": "no-cache", "User-Agent": "mynou-secret-audit",
                   "X-GitHub-Api-Version": "2022-11-28",
                   "Authorization": "Bearer " + self.token}
        request = urllib.request.Request(self.base + path, headers=headers)
        limit = MAX_ARCHIVE if archive else 8 * 1024 * 1024
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
        for page in range(1, 101):
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
        return {"state": "unavailable", "location": identity, "http": error.status}
    members, found = scan_archive(data, identity)
    return {"state": "scanned", "location": identity, "members": members, "findings": found}


def actions_logs(api):
    runs = list(api.pages("/actions/runs", "workflow_runs"))
    attempts, pending = [], []
    for run in runs:
        if run["status"] != "completed":
            pending.append("actions-run:" + str(run["id"]))
            continue
        number = run.get("run_attempt", 1)
        if type(number) is not int or not 1 <= number <= 100:
            raise AuditError("Invalid attempt inventory")
        attempts.extend((run["id"], a) for a in range(1, number + 1))
    api.budget.require(len(attempts))
    found, unavailable = [], []
    counts = {"scanned": 0, "no-runner-execution": 0}
    members = 0
    # These are deterministic CI I/O threads, never additional AI workers.
    with ThreadPoolExecutor(max_workers=4) as pool:
        # Submit only one bounded batch. An exception never leaves a queue
        # draining hundreds of further credentialed requests after failure.
        for start in range(0, len(attempts), 4):
            batch = attempts[start:start + 4]
            results = list(pool.map(lambda item: audit_attempt(api, item), batch))
            for result in results:
                if result["state"] == "unavailable":
                    unavailable.append(result["location"])
                else:
                    counts[result["state"]] += 1
                    members += result.get("members", 0)
                    found.extend(result.get("findings", []))
    return {"scope": "all completed visible workflow attempts at inventory time",
            "runs_enumerated": len(runs), "attempts_examined": len(attempts),
            "archives_scanned": counts["scanned"], "members_scanned": members,
            "attempts_without_execution": counts["no-runner-execution"],
            "pending_runs": pending, "coverage_gaps": unavailable,
            "findings": found[:100], "finding_count": len(found)}


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("scope", choices=["history", "logs"])
    parser.add_argument("--output", required=True)
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
            report = actions_logs(api)
            report["api_budget"] = api.budget.evidence()
        report["schema"] = 1
        report["status"] = ("findings" if report["finding_count"] else
                            "incomplete" if report["coverage_gaps"] else "passed")
        Path(args.output).write_text(json.dumps(report, indent=2) + "\n")
        print(json.dumps(report))
        return 0 if report["status"] == "passed" else 1
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
