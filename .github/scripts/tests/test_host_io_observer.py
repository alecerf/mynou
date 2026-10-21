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


def service_trace():
    """Outer UDP plus one declared loopback service: its listener, then a client."""
    return trace()[:3] + [event("allow_tcp", port=4321)] + trace()[3:] + [
        event("socket", fd=5, type=1), event("bind", fd=5, port=0),
        event("bound", fd=5, source=4321, port=0), event("socket", fd=6, type=1),
        event("connect", fd=6, source=0, port=4321),
        event("sendto", fd=6, family=0, source=50000, ip="", port=0),
        event("sendto", fd=9, family=0, source=4321, ip="", port=0)]


class ObservationTests(unittest.TestCase):
    def test_route_relay_authorization_is_an_exact_address_and_port(self):
        rows = trace()
        for index in (2, 6):
            rows[index]["ip"] = "127.0.0.2"
        self.assertEqual(observer.validate(rows)["sendto"], 1)
        for index, ip in ((2, "192.0.2.1"), (2, "127.0.0.3"), (6, "127.0.0.1")):
            changed = copy.deepcopy(rows)
            changed[index]["ip"] = ip
            with self.subTest(index=index, ip=ip), self.assertRaises(ValueError):
                observer.validate(changed)
        # A relay bound on .2 must declare that exact source, too.
        for index in (1, 4, 5):
            rows[index]["ip"] = "127.0.0.2"
        self.assertEqual(observer.validate(rows)["sendto"], 1)
        rows[1]["ip"] = "127.0.0.1"
        with self.assertRaises(ValueError):
            observer.validate(rows)

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

    def test_declared_ordinary_service_allows_only_its_loopback_listener_and_connections(self):
        rows = service_trace()
        counts = observer.validate(rows)
        self.assertEqual((counts["service_listen"], counts["service_connect"],
                          counts["service_send"]), (1, 1, 2))
        changes = [(3, "port", 0), (8, "family", 30), (8, "fd", -1), (9, "ip", "0.0.0.0"),
                   (9, "port", 80), (10, "source", 9999), (12, "ip", "192.0.2.1"),
                   (12, "port", 9999), (12, "family", 30), (12, "pid", 8),
                   (13, "family", 2), (13, "port", 9999), (13, "fd", 5),
                   (14, "source", 4322), (14, "port", 4321), (14, "pid", 8)]
        for index, key, value in changes:
            changed = copy.deepcopy(rows)
            changed[index][key] = value
            with self.subTest(index=index, key=key), self.assertRaises(ValueError):
                observer.validate(changed)

    def test_service_declaration_authorizes_no_lookup_udp_or_undeclared_process(self):
        additions = [[event("getaddrinfo", fd=-1)],
                     [event("sendto", fd=6, source=4321, port=4321)],
                     [event("socket", fd=7, type=1)],
                     [event("loaded", pid=8), event("socket", pid=8, fd=6, type=1),
                      event("connect", pid=8, fd=6, port=4321)],
                     [event("connect", fd=9, port=4321)],
                     # A descriptor reused after the service connection is unauthorized.
                     [event("socket", fd=6), event("sendto", fd=6, family=0, ip="", port=0)]]
        for extra in additions:
            with self.subTest(extra=extra), self.assertRaises(ValueError):
                observer.validate(service_trace() + extra)
        # Without a declaration, TCP stays refused exactly as before.
        rows = [r for r in service_trace() if r["op"] != "allow_tcp"]
        with self.assertRaises(ValueError):
            observer.validate(rows)

    def test_trace_reader_rejects_partial_json(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "trace"
            path.write_text('{"op":"loaded"}\n{"op":')
            with self.assertRaises(ValueError):
                observer.read_trace(path)


if __name__ == "__main__":
    unittest.main()
