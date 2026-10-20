"""An empty or permissive host-I/O trace must never look like isolation evidence."""
import copy
from pathlib import Path
import sys
import tempfile
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import host_io_observer as observer


def event(op, **extra):
    return {"op": op, "pid": 7, "fd": 4, "family": 2, "type": 2,
            "source": 1234, "ip": "127.0.0.1", "port": 1235, **extra}


def trace():
    return [event("loaded"), event("allow", port=1234), event("allow", port=1235),
            event("socket"), event("bind", port=0), event("bound", port=0), event("sendto")]


class ObservationTests(unittest.TestCase):
    def test_accepts_only_explicit_fixture_udp(self):
        self.assertEqual(observer.validate(trace())["sendto"], 1)
        # Declarations may arrive after setup/handshake; no early events are cut.
        rows = trace()
        self.assertEqual(observer.validate(rows[:1] + rows[3:] + rows[1:3])["socket"], 1)

    def test_refuses_missing_instrumentation_and_incomplete_observation(self):
        for rows in ([], trace()[1:], trace()[:3], trace()[:-1],
                     [r for r in trace() if r["op"] != "allow"],
                     [r for r in trace() if r["op"] != "bound"]):
            with self.subTest(rows=rows), self.assertRaises(ValueError):
                observer.validate(rows)

    def test_rejects_every_failed_or_successful_tcp_dns_and_unknown_attempt(self):
        for op in ("connect", "getaddrinfo", "sendmsg", "unknown"):
            with self.subTest(op=op), self.assertRaisesRegex(ValueError, "ordinary attempt"):
                observer.validate(trace() + [event(op)])

    def test_refuses_wrong_destinations_source_process_and_socket_kind(self):
        changes = [(3, "family", 30), (3, "type", 1), (3, "fd", -1),
                   (4, "ip", "0.0.0.0"), (4, "port", 1234),
                   (5, "source", 9999), (6, "source", 9999),
                   (6, "ip", "192.0.2.1"), (6, "port", 9999), (6, "pid", 8)]
        for index, key, value in changes:
            rows = copy.deepcopy(trace())
            rows[index][key] = value
            with self.subTest(index=index, key=key), self.assertRaises(ValueError):
                observer.validate(rows)

    def test_subprocesses_cannot_borrow_parent_authorization(self):
        rows = trace() + [event("loaded", pid=8), event("sendto", pid=8)]
        with self.assertRaisesRegex(ValueError, "datagram outside"):
            observer.validate(rows)

    def test_trace_reader_rejects_partial_json(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "trace"
            path.write_text('{"op":"loaded"}\n{"op":')
            with self.assertRaises(ValueError):
                observer.read_trace(path)


if __name__ == "__main__":
    unittest.main()
