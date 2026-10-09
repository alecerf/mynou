"""Original cache/ownership transport scenarios; execute exclusively in CI."""
import base64
from copy import deepcopy
import io
import json
import os
from pathlib import Path
import sys
import unittest
from unittest.mock import patch
import urllib.error
from urllib.parse import parse_qs, urlsplit

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import control
from github import GitHub
import lease
from test_organization import AT, BASE, CFG, HEAD, held

NOTES = "refs/notes/mynou-engineering"


class CachedAuthority:
    """Synthetic native storage/cache, not a reproduction of a provider outage."""
    def __init__(self):
        self.head = HEAD
        self.cached_head = HEAD
        self.states = {HEAD: held("triage")}
        self.trees = {}
        self.commits = {}
        self.requests = []
        self.cache_hits = 0
        self.ignore_revalidation = False
        self.race = False
        self.conflict = None

    def native(self, sha):
        return {"ref": NOTES, "object": {"type": "commit", "sha": sha}}

    def response(self, value):
        return io.BytesIO(json.dumps(value).encode())

    def open(self, request, timeout):
        route = urlsplit(request.full_url)
        prefix = "/repos/original/mynou/"
        if route.scheme != "https" or route.netloc != "api.github.com" or not route.path.startswith(prefix):
            raise AssertionError("Unexpected original fixture route")
        path = route.path.removeprefix(prefix)
        method = request.get_method()
        value = None if request.data is None else json.loads(request.data)
        self.requests.append((method, path, deepcopy(value)))
        if method == "GET" and path == "git/ref/notes/mynou-engineering":
            if request.get_header("Cache-control") == "no-cache" and not self.ignore_revalidation:
                observed = self.head
            else:
                observed = self.cached_head
                self.cache_hits += 1
            # A stale response can precede a fresh one, causing a guarded mismatch.
            self.cached_head = self.head
            return self.response(self.native(observed))
        if method == "GET" and path == "contents/state.json":
            observed = parse_qs(route.query)["ref"][0]
            raw = json.dumps(self.states[observed]).encode()
            return self.response({"encoding": "base64", "content": base64.b64encode(raw).decode()})
        if method == "POST" and path == "git/trees":
            entry, = value["tree"]
            if (entry["path"], entry["mode"], entry["type"]) != ("state.json", "100644", "blob"):
                raise AssertionError("Unexpected original fixture tree")
            identity = format(10 + len(self.trees), "040x")
            self.trees[identity] = json.loads(entry["content"])
            return self.response({"sha": identity})
        if method == "POST" and path == "git/commits":
            identity = format(100 + len(self.commits), "040x")
            self.commits[identity] = {"parents": value["parents"], "state": self.trees[value["tree"]]}
            if self.race:
                self.head = BASE
                self.states[BASE] = held("triage", "another-owner")
            return self.response({"sha": identity})
        if method == "PATCH" and path == "git/refs/notes/mynou-engineering":
            commit = self.commits[value["sha"]]
            if self.conflict or value["force"] is not False or commit["parents"] != [self.head]:
                raise urllib.error.HTTPError(request.full_url, self.conflict or 422,
                    "Original synthetic CAS conflict", {}, None)
            self.head = value["sha"]
            self.states[self.head] = deepcopy(commit["state"])
            # Native writes do not implicitly refresh the independently cached GET.
            return self.response(self.native(self.head))
        raise AssertionError("Unexpected original fixture operation")


class AuthorityTransport(unittest.TestCase):
    def client(self):
        cfg = dict(CFG, control_ref=NOTES)
        cfg.pop("control_branch")
        with patch.dict(os.environ, {"GH_TOKEN": "original-opaque-fixture-credential"}):
            client = GitHub(cfg)
        client.opener = CachedAuthority()
        return client

    def publication_checkpoint(self, client):
        head, state = control.read_state(client)
        value = lease.checkpoint(state, AT, "original-owner",
            "Original synthetic publication verified", "Release this same owner", HEAD, 3)
        value["checkpoint"]["publication"] = {
            "state": "published", "commit": HEAD, "version": "0.22.31",
            "at": lease.stamp(AT), "run_id": 40,
            "registry_proof": {"visibility": "private", "source": HEAD}}
        recorded = control.save(client, head, value, "Original publication checkpoint")
        return recorded, value["checkpoint"]

    def test_checkpoint_then_release_revalidates_and_retains_publication(self):
        client = self.client()
        recorded, checkpoint = self.publication_checkpoint(client)
        head, state = control.read_state(client)
        self.assertEqual(head, recorded)
        self.assertEqual(state["checkpoint"], checkpoint)
        released = control.save(client, head, lease.release(state, AT, "original-owner"),
            "Original publication release")
        head, state = control.read_state(client)
        self.assertEqual(head, released)
        self.assertIsNone(state["lease"])
        self.assertEqual(state["checkpoint"], checkpoint)
        self.assertEqual(client.opener.cache_hits, 0)
        self.assertEqual([c["parents"] for c in client.opener.commits.values()], [[HEAD], [recorded]])
        self.assertEqual(sum(r[0] == "PATCH" for r in client.opener.requests), 2)
        self.assertEqual(client.calls, len(client.opener.requests))
        self.assertLess(client.calls, 150)

    def test_changed_authority_is_rejected_before_any_write(self):
        client = self.client()
        head, state = control.read_state(client)
        proposed = lease.checkpoint(state, AT, "original-owner", "Original proposal", "Stop on loss")
        client.opener.head = BASE
        client.opener.states[BASE] = held("triage", "another-owner")
        with self.assertRaisesRegex(RuntimeError, "Execution authority changed"):
            control.save(client, head, proposed, "Original stale owner")
        self.assertEqual(client.opener.head, BASE)
        self.assertFalse(any(r[0] != "GET" for r in client.opener.requests))
        self.assertEqual(client.opener.states[BASE]["lease"]["id"], "another-owner")

    def test_another_owner_on_the_same_product_source_cannot_be_released(self):
        client = self.client()
        client.opener.head = BASE
        client.opener.states[BASE] = held("triage", "another-owner")
        _, state = control.read_state(client)
        self.assertEqual(state["lease"]["commit"], HEAD)
        with self.assertRaises(ValueError):
            lease.release(state, AT, "original-owner")
        self.assertFalse(any(r[0] != "GET" for r in client.opener.requests))
        self.assertEqual(client.opener.head, BASE)

    def test_real_cas_conflicts_preserve_the_winner_without_force_or_retry(self):
        for code in [None, 409, 422]:
            client = self.client()
            client.opener.race = True
            client.opener.conflict = code
            head, state = control.read_state(client)
            proposed = lease.checkpoint(state, AT, "original-owner", "Original proposal", "Stop on loss")
            with self.subTest(code=code), self.assertRaisesRegex(RuntimeError, "CAS lost"):
                control.save(client, head, proposed, "Original competing writer")
            self.assertEqual(client.opener.head, BASE)
            self.assertEqual(client.opener.states[BASE]["lease"]["id"], "another-owner")
            updates = [r for r in client.opener.requests if r[0] == "PATCH"]
            self.assertEqual(len(updates), 1)
            self.assertIs(updates[0][2]["force"], False)
            self.assertEqual([c["parents"] for c in client.opener.commits.values()], [[HEAD]])

    def test_ignored_revalidation_still_fails_closed_and_preserves_checkpoint(self):
        client = self.client()
        client.opener.ignore_revalidation = True
        recorded, checkpoint = self.publication_checkpoint(client)
        head, stale = control.read_state(client)
        self.assertNotEqual(head, recorded)
        with self.assertRaisesRegex(RuntimeError, "Execution authority changed"):
            control.save(client, head, lease.release(stale, AT, "original-owner"),
                "Original stale release")
        self.assertEqual(client.opener.head, recorded)
        self.assertEqual(client.opener.states[recorded]["checkpoint"], checkpoint)
        self.assertEqual(client.opener.states[recorded]["lease"]["id"], "original-owner")
        self.assertEqual(sum(r[0] == "PATCH" for r in client.opener.requests), 1)


if __name__ == "__main__":
    unittest.main()
