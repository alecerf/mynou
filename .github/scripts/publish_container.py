"""CI-only private GHCR publication using Python std and the runner's Docker/gh."""
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import tempfile


class CommandError(RuntimeError):
    def __init__(self, args, stderr):
        super().__init__("Command failed: " + args[0] + " " + args[1])
        self.stderr = stderr


def command(args, input=None):
    result = subprocess.run(args, input=input, text=True, capture_output=True)
    if result.returncode:
        raise CommandError(args, result.stderr)
    return result.stdout


def file_digest(path):
    with open(path, "rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


class Publisher:
    def __init__(self, version, run=command):
        self.version = version
        self.run = run
        self.repo = os.environ["GITHUB_REPOSITORY"]
        self.source = os.environ["GITHUB_SHA"]
        self.image = "ghcr.io/" + self.repo.lower()
        repository_id = os.environ.get("GITHUB_REPOSITORY_ID", "")
        if (self.repo != "alecerf/mynou"
                or not re.fullmatch(r"[0-9]+\.[0-9]+\.[0-9]+", version)
                or not re.fullmatch(r"[0-9a-f]{40}", self.source)
                or not re.fullmatch(r"[1-9][0-9]{0,18}", repository_id)
                or os.environ.get("GITHUB_REF") != "refs/heads/trunk"
                or os.environ.get("GITHUB_EVENT_NAME") not in ("push", "workflow_dispatch")):
            raise ValueError("Publication requires exact trusted-default source")
        self.repository_id = int(repository_id)
        if self.repository_id >= 2 ** 63:
            raise ValueError("Invalid publishing repository identity")
        self.package_id = None
        self.package_association = "not_exposed"
        self.package_path = "/users/alecerf/packages/container/mynou"

    def api_object(self, path):
        raw = self.run(["gh", "api", "--method", "GET", path])
        if len(raw) > 65_536:
            raise ValueError("Publication metadata exceeds its bound")
        try:
            value = json.loads(raw)
        except ValueError:
            raise ValueError("Publication metadata is malformed") from None
        if not isinstance(value, dict):
            raise ValueError("Publication metadata requires an object")
        return value

    def repository_context(self):
        try:
            repository = self.api_object("/repos/" + self.repo)
        except CommandError:
            raise RuntimeError("Cannot verify the publishing repository") from None
        if (repository.get("full_name") != self.repo
                or type(repository.get("id")) is not int
                or repository["id"] != self.repository_id
                or repository.get("private") is not True):
            raise ValueError("Publication requires the exact private repository")

    def private_package(self, allow_missing=False):
        try:
            package = self.api_object(self.package_path)
        except CommandError as error:
            if allow_missing and re.search(r"\bHTTP 404\b", error.stderr):
                return
            raise RuntimeError("Cannot verify private package identity") from None
        owner = package.get("owner")
        identity = package.get("id")
        if (package.get("visibility") != "private"
                or package.get("name") != "mynou"
                or package.get("package_type") != "container"
                or not isinstance(owner, dict) or owner.get("login") != "alecerf"
                or type(identity) is not int or not 0 < identity < 2 ** 63):
            raise ValueError("GHCR package must have the exact private owner/name/type identity")
        linked = package.get("repository")
        if linked is not None and (not isinstance(linked, dict)
                or linked.get("full_name") != self.repo
                or ("id" in linked and (type(linked["id"]) is not int
                                       or linked["id"] != self.repository_id))):
            raise ValueError("GHCR package reports a conflicting repository association")
        # Container metadata can omit this optional REST field. Do not turn
        # omission into a claim of linkage: exact private source/package IDs,
        # scoped-token access and verified pulled image provenance are required.
        if self.package_id is not None and identity != self.package_id:
            raise ValueError("Package identity changed during publication")
        self.package_id = identity
        self.package_association = "reported-match" if linked is not None else "not_exposed"

    def existing(self, ref):
        try:
            self.run(["docker", "manifest", "inspect", ref])
        except CommandError as error:
            if re.search(r"\b(manifest unknown|no such manifest)\b", error.stderr):
                return False
            raise RuntimeError("Registry lookup failed; absence is not established") from None
        self.run(["docker", "pull", ref])
        if self.inspect(ref)["Id"] != self.expected_id:
            raise ValueError("Refusing to replace a conflicting published image")
        return True

    def inspect(self, ref):
        images = json.loads(self.run(["docker", "image", "inspect", ref]))
        if len(images) != 1:
            raise ValueError("Expected exactly one image")
        image = images[0]
        labels = image.get("Config", {}).get("Labels") or {}
        if (image.get("Os") != "linux" or image.get("Architecture") != "amd64"
                or image.get("Config", {}).get("User") != "1000:1000"
                or labels.get("org.opencontainers.image.source") != "https://github.com/" + self.repo
                or labels.get("org.opencontainers.image.revision") != self.source
                or labels.get("org.opencontainers.image.version") != self.version):
            raise ValueError("Image identity or runtime policy differs from validated source")
        return image

    def publish(self, archive, binary, root):
        self.repository_context()
        self.private_package(allow_missing=True)
        self.run(["docker", "load", "--input", str(archive)])
        local = "mynou:" + self.version
        self.expected_id = self.inspect(local)["Id"]
        self.run(["docker", "login", "ghcr.io", "--username",
                  os.environ["GITHUB_ACTOR"], "--password-stdin"],
                 input=os.environ["GH_TOKEN"] + "\n")
        refs = [self.image + ":sha-" + self.source, self.image + ":" + self.version]
        # Inspect every tag before any push. GitHub serializes this workflow;
        # outside writers can still race because registries provide no tag CAS.
        present = [self.existing(ref) for ref in refs]
        for ref, exists in zip(refs, present):
            if not exists and not self.existing(ref):
                self.run(["docker", "tag", local, ref])
                self.run(["docker", "push", ref])
        self.private_package()
        self.run(["docker", "pull", refs[1]])
        image = self.inspect(refs[1])
        if image["Id"] != self.expected_id:
            raise ValueError("Published version changed during verification")
        digests = {value for value in image.get("RepoDigests", [])
                   if re.fullmatch(re.escape(self.image) + r"@sha256:[0-9a-f]{64}", value)}
        if len(digests) != 1:
            raise ValueError("A unique registry digest is required")
        pinned = digests.pop()
        self.run(["docker", "pull", pinned])
        if self.inspect(pinned)["Id"] != self.expected_id:
            raise ValueError("Registry digest does not preserve the checked image")
        container = self.run(["docker", "create", pinned]).strip()
        if not re.fullmatch(r"[0-9a-f]{64}", container):
            raise ValueError("Invalid container identity")
        copied = root / "published-mynou"
        try:
            self.run(["docker", "cp", container + ":/mynou", str(copied)])
        finally:
            self.run(["docker", "rm", container])
        if file_digest(copied) != file_digest(binary):
            raise ValueError("Published executable differs from the checked binary")
        demo = root / "demo"
        demo.mkdir(mode=0o777)
        demo.chmod(0o777)
        result = json.loads(self.run([
            "docker", "run", "--rm", "--network", "none", "--read-only",
            "--cap-drop", "ALL", "--security-opt", "no-new-privileges:true",
            "--mount", "type=bind,src=" + str(demo) + ",dst=/data",
            pinned, "demo", "--dir", "/data/demo",
        ]))
        if result.get("job", {}).get("state") != "ready" or result.get("plex_scan_confirmed") is not True:
            raise ValueError("Published container demonstration failed")
        return {"image": pinned, "version": self.version, "source": self.source,
                "visibility": "private", "binary_sha256": file_digest(binary),
                "repository": self.repo, "repository_id": self.repository_id,
                "package_id": self.package_id, "repository_association": self.package_association}


def main():
    import sys
    if len(sys.argv) != 4:
        raise ValueError("Usage: publish_container.py IMAGE_TAR_GZ VERSION CHECKED_BINARY")
    publisher = Publisher(sys.argv[2])
    # Credentials never enter a shared Docker config or persist in artifacts.
    with tempfile.TemporaryDirectory(prefix="mynou-publication-", dir=os.environ["RUNNER_TEMP"]) as work:
        root = Path(work)
        config = root / "docker"
        config.mkdir(mode=0o700)
        os.environ["DOCKER_CONFIG"] = str(config)
        proof = publisher.publish(sys.argv[1], Path(sys.argv[3]), root)
        Path(os.environ["RUNNER_TEMP"], "mynou-image.json").write_text(json.dumps(proof) + "\n")
        with open(os.environ["GITHUB_OUTPUT"], "a") as output:
            output.write("image_ref=" + proof["image"] + "\n")
        print(json.dumps(proof, sort_keys=True))


if __name__ == "__main__":
    try:
        main()
    except (ValueError, RuntimeError, OSError, KeyError) as error:
        raise SystemExit("Container publication failed: " + str(error)) from None
