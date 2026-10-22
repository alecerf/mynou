"""Negative fixtures: every inventoried construct, alias and inventory drift must fail the check."""
import contextlib
import importlib.util
import io
from pathlib import Path
import tempfile
import unittest

spec = importlib.util.spec_from_file_location("network_inventory", Path(__file__).parents[1] / "network_inventory.py")
inventory = importlib.util.module_from_spec(spec)
spec.loader.exec_module(inventory)

LIB = "mod net;\nmod torrent;\nmod plex;\n"
NET = """
use std::net::TcpStream;
pub struct HttpClient;
impl HttpClient { pub fn new() -> Self { HttpClient } }
pub(crate) fn blocking(stream: &TcpStream) -> Result<(), ()> { Ok(()) }
pub(crate) struct DeadlineStream;
impl DeadlineStream { pub(crate) fn new(stream: TcpStream, deadline: u8) -> Self { DeadlineStream } }
"""
CLEAN = """
use crate::net::HttpClient;
// TcpStream::connect in a comment, and /* UdpSocket */ in a block comment.
fn fetch(connector: Connector) -> &'static str {
    let _client = HttpClient::new().with_max_body(10).with_connector(connector);
    let _bytes = [0u8; 20];
    let _raw = r#"TcpListener::bind("0.0.0.0:0")"#;
    let _c = '"';
    "to_socket_addrs"
}
#[cfg(test)]
mod tests {
    use std::net::TcpListener;
    #[test]
    fn trap() { TcpListener::bind("127.0.0.1:0").unwrap(); }
}
#[cfg(test)]
mod torrent_tests;
"""
SITES = {
    "TcpStream": "fn peer() { let _ = std::net::TcpStream::connect(\"127.0.0.1:1\"); }",
    "TcpListener": "fn listen() { let _ = std::net::TcpListener::bind(\"0.0.0.0:0\"); }",
    "UdpSocket": "fn dht(socket: &std::net::UdpSocket) {}",
    "ToSocketAddrs": "fn names<A: std::net::ToSocketAddrs>(a: A) {}",
    "to_socket_addrs": "fn names(host: &str) { let _ = host.to_socket_addrs(); }",
    "HttpClient::new": "fn fetch() { let _ = crate::net::HttpClient::new().with_max_body(1); }",
    "std::net::*": "use std::net::*;",
    "net::blocking": "fn adopt(s: Stream) { crate::net::blocking(&s); }",
    "DeadlineStream::new": "fn wrap(s: Stream) { crate::net::DeadlineStream::new(s, 1); }",
}


class Fixture:
    def __init__(self, case, torrent, extra=None):
        self.directory = tempfile.TemporaryDirectory()
        case.addCleanup(self.directory.cleanup)
        self.root = Path(self.directory.name)
        files = {"src/lib.rs": LIB, "src/net.rs": NET, "src/torrent.rs": torrent,
                 "src/torrent/torrent_tests.rs": "fn helper() { std::net::UdpSocket::bind(\"x\"); }",
                 "src/plex.rs": "fn plex() { std::net::TcpStream::connect(\"plex:32400\"); }"}
        files.update(extra or {})
        for name, text in files.items():
            (self.root / name).parent.mkdir(parents=True, exist_ok=True)
            (self.root / name).write_text(text)

    def scan(self):
        return inventory.scan(self.root)[0]

    def check(self, inventory_text):
        path = self.root / "inventory.tsv"
        path.write_text(inventory_text)
        with contextlib.redirect_stderr(io.StringIO()) as errors, contextlib.redirect_stdout(io.StringIO()):
            status = inventory.main(["--root", str(self.root), "--inventory", str(path)])
        return status, errors.getvalue()


class Constructs(unittest.TestCase):
    def test_comments_strings_connectors_and_test_code_are_not_sites(self):
        fixture = Fixture(self, CLEAN)
        self.assertEqual(fixture.scan(), {})
        self.assertEqual(fixture.check(inventory.render({}))[0], 0)

    def test_each_direct_construct_fails_an_empty_inventory(self):
        for construct, source in SITES.items():
            with self.subTest(construct=construct):
                fixture = Fixture(self, CLEAN + source)
                self.assertIn(construct, {key[2] for key in fixture.scan()})
                status, errors = fixture.check(inventory.render({}))
                self.assertEqual(status, 1)
                self.assertIn(f"new direct site: src/torrent.rs", errors)
                self.assertIn(construct, errors)

    def test_vpn_control_plane_cannot_open_an_unrestricted_torrent_socket(self):
        fixture = Fixture(self, CLEAN, {
            "src/lib.rs": LIB + "mod vpn;\n",
            "src/vpn.rs": SITES["TcpStream"],
        })
        status, errors = fixture.check(inventory.render(fixture.scan()))
        self.assertEqual(status, 1)
        self.assertIn("direct site outside Direct: src/vpn.rs", errors)

    def test_discovery_scopes_reject_unrestricted_sockets_even_when_inventoried(self):
        for module, file in (("indexers", "src/indexers.rs"), ("source_network", "src/source_network.rs"), ("irc", "src/irc/mod.rs")):
            with self.subTest(module=module):
                fixture = Fixture(self, CLEAN, {"src/lib.rs": LIB + f"mod {module};\n", file: SITES["TcpStream"]})
                status, errors = fixture.check(inventory.render(fixture.scan()))
                self.assertEqual(status, 1)
                self.assertIn("direct site outside Direct: " + file, errors)

    def test_discovery_direct_boundary_cannot_authorize_new_items(self):
        fixture = Fixture(self, CLEAN, {
            "src/lib.rs": LIB + "mod source_network;\n",
            "src/source_network.rs": "mod direct;\n",
            "src/source_network/direct.rs": "fn unexpected() { std::net::TcpStream::connect(\"x\"); }",
        })
        status, errors = fixture.check(inventory.render(fixture.scan()))
        self.assertEqual(status, 1)
        self.assertIn("direct site outside Direct: src/source_network/direct.rs", errors)

    def test_aliases_report_the_canonical_construct(self):
        cases = {"use std::net::{TcpStream as Socket};\nfn peer(s: Socket) {}": ("peer", "TcpStream"),
                 "type Socket = std::net::TcpStream;\nfn peer(s: Socket) {}": ("peer", "TcpStream"),
                 "use crate::net::HttpClient as Client;\nfn fetch() { Client::new(); }": ("fetch", "HttpClient::new")}
        for source, (item, construct) in cases.items():
            with self.subTest(source=source):
                self.assertIn(("src/torrent.rs", item, construct), Fixture(self, CLEAN + source).scan())

    def test_only_exact_test_configuration_is_excluded(self):
        source = "#[cfg(not(test))]\nfn live() { std::net::TcpStream::connect(\"x\"); }\n" \
                 "#[cfg(any(test, feature = \"x\"))]\nfn either() { std::net::UdpSocket::bind(\"x\"); }"
        self.assertEqual(set(Fixture(self, CLEAN + source).scan()),
                         {("src/torrent.rs", "live", "TcpStream"), ("src/torrent.rs", "either", "UdpSocket")})

    def test_out_of_scope_alias_is_followed(self):
        extra = {"src/plex.rs": "pub type Socket = std::net::TcpStream;"}
        found = Fixture(self, CLEAN + "fn peer(s: crate::plex::Socket) {}", extra).scan()
        self.assertEqual(found, {("src/torrent.rs", "peer", "TcpStream"): 1})

    def test_sites_are_keyed_by_enclosing_function(self):
        source = "impl Peer {\n    fn connect(key: [u8; 20]) -> impl Iterator<Item = u8> {\n" \
                 "        std::net::TcpStream::connect(\"x\");\n        None.into_iter()\n    }\n}"
        self.assertEqual(Fixture(self, CLEAN + source).scan(), {("src/torrent.rs", "Peer::connect", "TcpStream"): 1})

    def test_out_of_scope_and_test_module_sockets_are_ignored(self):
        fixture = Fixture(self, CLEAN, {"src/torrent/torrent_tests.rs": "fn x() { std::net::TcpStream::connect(\"x\"); }"})
        self.assertEqual(fixture.scan(), {})

    def test_path_declared_scope_modules_are_scanned(self):
        source = CLEAN + '#[path = "torrent/wire.rs"]\nmod wire;\n'
        extra = {"src/torrent/wire.rs": "fn peer() { std::net::TcpStream::connect(\"x\"); }"}
        self.assertEqual(Fixture(self, source, extra).scan(), {("src/torrent/wire.rs", "peer", "TcpStream"): 1})


class Drift(unittest.TestCase):
    def setUp(self):
        self.fixture = Fixture(self, CLEAN + SITES["TcpStream"])
        self.current = self.fixture.scan()

    def test_matching_inventory_cannot_authorize_a_site_outside_direct(self):
        status, errors = self.fixture.check(inventory.render(self.current))
        self.assertEqual(status, 1)
        self.assertIn("direct site outside Direct", errors)

    def test_only_the_direct_implementation_can_match_its_inventory(self):
        fixture = Fixture(self, "mod transport;", {"src/torrent/transport.rs":
            "impl Direct { fn connect() { std::net::TcpStream::connect(\"x\"); } }"})
        self.assertEqual(fixture.check(inventory.render(fixture.scan()))[0], 0)

    def test_increased_site_count_fails(self):
        fixture = Fixture(self, CLEAN + SITES["TcpStream"].replace(");", "); std::net::TcpStream::connect(\"y\");"))
        status, errors = fixture.check(inventory.render(self.current))
        self.assertEqual(status, 1)
        self.assertIn("(1 -> 2)", errors)

    def test_stale_entry_fails_after_conversion(self):
        stale = dict(self.current)
        stale[("src/torrent.rs", "removed", "UdpSocket")] = 1
        status, errors = self.fixture.check(inventory.render(stale))
        self.assertEqual(status, 1)
        self.assertIn("stale inventory entry: src/torrent.rs | removed | UdpSocket", errors)

    def test_committed_inventory_matches_the_repository(self):
        counts, _ = inventory.scan()
        self.assertEqual(inventory.compare(counts, inventory.read_inventory(inventory.INVENTORY)), [])


if __name__ == "__main__":
    unittest.main()
