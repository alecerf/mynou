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
# Content-addressed reads whose answer can never change: one fetch per process.
IMMUTABLE = re.compile(r"/repos/[^/]+/[^/]+/(?:git/commits/[0-9a-f]{40}"
                       r"|compare/[0-9a-f]{40}\.\.\.[0-9a-f]{40}"
                       r"|contents/[^?#]+\?ref=[0-9a-f]{40})")


class APIError(RuntimeError):
    def __init__(self, status):
        self.status = status
        super().__init__(f"GitHub request failed with HTTP {status}; refresh state before retrying")


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        raise APIError(code)


class StorageRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        return None


def artifact_destination(value):
    try:
        parsed = urllib.parse.urlsplit(value)
        host = parsed.hostname or ""
        allowed = (parsed.scheme == "https" and not parsed.username and not parsed.password
            and parsed.port in (None, 443) and not parsed.fragment
            and (host.endswith(".blob.core.windows.net") or host.endswith(".githubusercontent.com")))
    except (TypeError, ValueError):
        allowed = False
    if not allowed:
        raise ValueError("Unsupported publication artifact storage destination")
    return value


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
    """One configured authority; legacy workers must see the same branch."""
    legacy = None
    if "control_branch" in cfg:
        legacy = cfg["control_branch"]
        if not reference(legacy).startswith("refs/heads/"):
            raise ValueError("Legacy control_branch must identify a branch")
    if "control_ref" in cfg:
        current = cfg["control_ref"]
        if not isinstance(current, str) or not current.startswith("refs/"):
            raise ValueError("control_ref must be fully qualified")
        full = reference(current)
        if legacy is not None and reference(legacy) != full:
            raise ValueError("Control branch and reference identify different authorities")
    elif legacy is not None:
        current, full = legacy, reference(legacy)
    else:
        raise ValueError("A canonical control reference is required")
    if "default_branch" in cfg:
        default = reference(cfg["default_branch"])
        if not default.startswith("refs/heads/"):
            raise ValueError("Default branch must identify a branch")
        if full == default:
            raise ValueError("Product default branch cannot store engineering control")
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


def team_policy(cfg):
    """Opt-in parallel team bound. `max_active_agents` stays the serial default of 1."""
    value = cfg.get("team")
    if value is None:
        return None
    if not isinstance(value, dict) or set(value) != {"issue", "max_active_agents"} \
            or type(value["issue"]) is not int or value["issue"] < 1 \
            or type(value["max_active_agents"]) is not int or not 2 <= value["max_active_agents"] <= 8:
        raise ValueError("Invalid team policy; it needs the reviewed Issue and a cap of 2-8")
    return value


def config():
    value = json.loads((ROOT / ".github/engineering.json").read_text())
    if value.get("schema") != 1 or value.get("max_active_agents") != 1:
        raise ValueError("Unsupported organization schema or concurrency")
    team_policy(value)
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
        self.immutable = {}

    def consume_request(self):
        self.calls += 1
        if self.calls > 150:
            raise ValueError("Bounded GitHub request budget exhausted; checkpoint and resume")

    def request(self, method, path, value=None):
        if not path.startswith("/") or path.startswith("//"):
            raise ValueError("GitHub paths must be absolute API paths")
        # Refs, PRs, runs and other mutable state are always re-read; only
        # commit-addressed objects are reused, as fresh copies.
        cacheable = method == "GET" and IMMUTABLE.fullmatch(path) is not None
        if cacheable and path in self.immutable:
            return json.loads(self.immutable[path])
        self.consume_request()
        data = None if value is None else json.dumps(value).encode()
        request = urllib.request.Request("https://api.github.com" + path, data=data, method=method,
            headers={"Authorization": "Bearer " + self.token, "Accept": "application/vnd.github+json",
                "X-GitHub-Api-Version": "2022-11-28", "Content-Type": "application/json",
                "User-Agent": "mynou-engineering"})
        # Revalidate authority and gate observations after writes. A real head
        # mismatch still fails closed; this directive does not retry an API call.
        if method == "GET":
            request.add_header("Cache-Control", "no-cache")
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
        if cacheable and raw:
            self.immutable[path] = raw
        return json.loads(raw) if raw else None

    def publication_archive(self, artifact_id):
        """Read one small fixed native artifact; never forward auth to signed storage."""
        if type(artifact_id) is not int or artifact_id <= 0:
            raise ValueError("Invalid native publication artifact identity")
        self.consume_request()
        opener = urllib.request.build_opener(StorageRedirect())
        request = urllib.request.Request("https://api.github.com/repos/" + self.repo
            + "/actions/artifacts/" + str(artifact_id) + "/zip", headers={
                "Authorization": "Bearer " + self.token, "Accept": "application/vnd.github+json",
                "X-GitHub-Api-Version": "2022-11-28", "User-Agent": "mynou-engineering"})
        limit = 64 * 1024
        try:
            try:
                response = opener.open(request, timeout=30)
            except urllib.error.HTTPError as redirect:
                if redirect.code != 302:
                    raise APIError(redirect.code) from None
                destination = artifact_destination(redirect.headers.get("Location", ""))
                self.consume_request()
                response = opener.open(urllib.request.Request(destination), timeout=30)
            with response:
                raw = response.read(limit + 1)
        except urllib.error.HTTPError as error:
            raise APIError(error.code) from None
        except urllib.error.URLError:
            raise RuntimeError("Publication artifact transport unavailable") from None
        except OSError:
            raise RuntimeError("Publication artifact transport unavailable") from None
        if len(raw) > limit:
            raise ValueError("Publication artifact exceeds the bounded size")
        return raw

    def rest(self, method, path, value=None):
        root = "/repos/" + self.repo
        return self.request(method, root if path == "" else root + "/" + path, value)

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

