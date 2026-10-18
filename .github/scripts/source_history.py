"""Required source-history scan; bounded, redacted and Python std-only."""
import argparse
import json
from pathlib import Path
import re
import subprocess
import sys

MAX_OBJECT = 8 * 1024 * 1024


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
        process.stdin.close()
        process.stdout.close()
    return {"scope": "all reachable fetched refs and their Git objects",
            "objects_scanned": count, "findings": found[:100],
            "finding_count": len(found), "coverage_gaps": []}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", required=True)
    args = parser.parse_args()
    try:
        report = git_history()
        report["schema"] = 1
        report["status"] = "findings" if report["finding_count"] else "passed"
    except Exception as error:
        # Exception messages and raw Git contents may contain credentials.
        report = {"schema": 1, "status": "error", "error_type": type(error).__name__}
    Path(args.output).write_text(json.dumps(report) + "\n")
    print(json.dumps(report))
    return 0 if report["status"] == "passed" else 1


if __name__ == "__main__":
    sys.exit(main())
