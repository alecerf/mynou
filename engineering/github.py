"""Small bounded GitHub REST/GraphQL administration client; Python std only."""
import base64
import json
import os
from pathlib import Path
import re
import urllib.error
import urllib.parse
import urllib.request

ROOT = Path(__file__).resolve().parent.parent
MAX_RESPONSE = 4 * 1024 * 1024


class APIError(RuntimeError):
    def __init__(self, status):
        self.status = status
        super().__init__(f"GitHub request failed with HTTP {status}; refresh state before retrying")


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        raise APIError(code)


def reference(value):
    """Normalize only branch/notes references; never admit release tag writes."""
    if not isinstance(value, str) or not 1 <= len(value) <= 200:
        raise ValueError("Invalid Git reference")
    full = value if value.startswith("refs/") else "refs/heads/" + value
    if not full.startswith(("refs/heads/", "refs/notes/")):
        raise ValueError("Only branch or notes references are supported")
    tail = full.split("/", 2)[2]
    if not re.fullmatch(r"[A-Za-z0-9_/-][A-Za-z0-9_./-]{0,159}", tail) or ".." in tail:
        raise ValueError("Invalid Git reference name")
    if any(not part or part.startswith(".") or part.endswith((".", ".lock")) for part in tail.split("/")):
        raise ValueError("Invalid Git reference component")
    return full


def control_reference(cfg):
    """One configured authority; a legacy alias must identify the same ref."""
    if "control_ref" not in cfg:
        if "control_branch" not in cfg:
            raise ValueError("A canonical control reference is required")
        reference(cfg["control_branch"])
        return cfg["control_branch"]
    current = cfg["control_ref"]
    if not isinstance(current, str) or not current.startswith("refs/"):
        raise ValueError("control_ref must be fully qualified")
    full = reference(current)
    if "control_branch" in cfg and reference(cfg["control_branch"]) != full:
        raise ValueError("Control branch and reference identify different authorities")
    return current


def ref_commit(value, expected_ref):
    """Reject prefix matches, annotated tags and malformed native ref objects."""
    if not isinstance(value, dict) or value.get("ref") != expected_ref:
        raise ValueError("GitHub returned a different or ambiguous reference")
    obj = value.get("object")
    if not isinstance(obj, dict) or obj.get("type") != "commit":
        raise ValueError("Control reference must point directly to a commit")
    sha = obj.get("sha")
    if not isinstance(sha, str) or not re.fullmatch(r"[0-9a-f]{40}", sha):
        raise ValueError("GitHub reference lacks an exact commit identity")
    return sha


def config():
    value = json.loads((ROOT / ".github/engineering.json").read_text())
    if value.get("schema") != 1 or value.get("max_active_agents") != 1:
        raise ValueError("Unsupported organization schema or concurrency")
    if not re.fullmatch(r"[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+", value["repository"]):
        raise ValueError("Invalid GitHub repository")
    control_reference(value)
    return value


class GitHub:
    def __init__(self, cfg=None):
        self.cfg = cfg or config()
        self.repo = self.cfg["repository"]
        self.token = os.environ.get("GH_TOKEN") or os.environ.get("GITHUB_TOKEN")
        if not self.token:
            raise ValueError("An authorized GitHub token is unavailable")
        self.opener = urllib.request.build_opener(NoRedirect())
        self.calls = 0

    def request(self, method, path, value=None):
        if not path.startswith("/") or path.startswith("//"):
            raise ValueError("GitHub paths must be absolute API paths")
        self.calls += 1
        if self.calls > 150:
            raise ValueError("Bounded GitHub request budget exhausted; checkpoint and resume")
        data = None if value is None else json.dumps(value).encode()
        request = urllib.request.Request("https://api.github.com" + path, data=data, method=method,
            headers={"Authorization": "Bearer " + self.token, "Accept": "application/vnd.github+json",
                "X-GitHub-Api-Version": "2022-11-28", "Content-Type": "application/json",
                "User-Agent": "mynou-engineering"})
        try:
            with self.opener.open(request, timeout=30) as response:
                raw = response.read(MAX_RESPONSE + 1)
        except urllib.error.HTTPError as error:
            # Never print request headers, tokens, response data or hostile input.
            raise APIError(error.code) from None
        except urllib.error.URLError:
            raise RuntimeError("GitHub transport unavailable; preserve state and retry later") from None
        if len(raw) > MAX_RESPONSE:
            raise ValueError("GitHub response exceeds the bounded size")
        return json.loads(raw) if raw else None

    def rest(self, method, path, value=None):
        return self.request(method, "/repos/" + self.repo + "/" + path, value)

    def pages(self, path, key=None):
        result = []
        for page in range(1, 11):
            separator = "&" if "?" in path else "?"
            value = self.rest("GET", path + separator + f"per_page=100&page={page}")
            items = value[key] if key else value
            if not isinstance(items, list):
                raise ValueError("Unexpected paginated GitHub response")
            result.extend(items)
            if len(items) < 100:
                return result
        raise ValueError("GitHub pagination bound reached; narrow the work scope")

    def graphql(self, query, variables=None):
        value = self.request("POST", "/graphql", {"query": query, "variables": variables or {}})
        if value.get("errors"):
            raise RuntimeError("GitHub GraphQL operation unavailable; do not assume it succeeded")
        return value["data"]

    def file(self, path, ref):
        value = self.rest("GET", "contents/" + urllib.parse.quote(path, safe="/")
            + "?ref=" + urllib.parse.quote(ref, safe=""))
        if value.get("encoding") != "base64":
            raise ValueError("GitHub file content is not inline base64")
        raw = base64.b64decode(value["content"])
        if len(raw) > 64 * 1024:
            raise ValueError("Control file exceeds 64 KiB")
        return json.loads(raw)

    def ref(self, location):
        full = reference(location)
        path = urllib.parse.quote(full.removeprefix("refs/"), safe="/")
        return ref_commit(self.rest("GET", "git/ref/" + path), full)

    def cas_file(self, location, expected, path, value, message):
        # A sibling commit cannot replace a concurrently advanced ref without force.
        # GitHub's non-force fast-forward update is the atomic arbitration point.
        full = reference(location)
        if not isinstance(expected, str) or not re.fullmatch(r"[0-9a-f]{40}", expected):
            raise ValueError("CAS requires an exact observed commit")
        if self.ref(location) != expected:
            raise RuntimeError("Execution lease changed; stop and reconstruct state")
        content = json.dumps(value, indent=2) + "\n"
        if len(content.encode()) > 64 * 1024:
            raise ValueError("Control state exceeds 64 KiB")
        tree = self.rest("POST", "git/trees", {"tree": [
            {"path": path, "mode": "100644", "type": "blob", "content": content}]})
        commit = self.rest("POST", "git/commits", {"message": message, "tree": tree["sha"], "parents": [expected]})
        sha = commit.get("sha")
        if not isinstance(sha, str) or not re.fullmatch(r"[0-9a-f]{40}", sha):
            raise ValueError("Created control commit lacks an exact identity")
        try:
            updated = self.rest("PATCH", "git/refs/" + urllib.parse.quote(full.removeprefix("refs/"), safe="/"),
                {"sha": sha, "force": False})
        except APIError as error:
            if error.status in (409, 422):
                raise RuntimeError("Execution lease CAS lost; stop without force or blind retry") from None
            raise
        if ref_commit(updated, full) != sha:
            raise RuntimeError("Control update response disagrees; stop and reconstruct remote state")
        return sha
