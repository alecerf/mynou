# Explicit HTTP transport connectors

The Rust library's `net::HttpClient::with_connector` accepts an
`Arc<dyn HttpConnector>`. The connector receives the parsed destination host,
port and absolute `Instant` deadline, and returns a `Box<dyn NetworkStream>`.
The stream implements `Read`, `Write`, `Send`, `set_read_timeout` and
`set_write_timeout`. `TcpStream` implements this interface; a virtual transport
can supply its own implementation without exposing a physical socket.

For example, after constructing a connector implementing that contract:

```rust
let client = mynou::net::HttpClient::new()
    .with_connector(connector)
    .with_timeout(std::time::Duration::from_secs(10));
let response = client.get("https://tracker.example/announce")?;
```

Every hop uses the same connector and absolute deadline, including redirects.
On this path HTTP does not call the host resolver, create a TCP connection,
consult environment proxies or fall back to the default connection path after
an error. HTTPS performs the existing authenticated TLS handshake on the
returned stream, using the URL hostname for certificate authentication and
SNI. Trust anchors still come from `MYNOU_CA_FILE` or `/etc/ssl/cert.pem`.
Response bounds, HTTP framing checks, cross-origin credential removal, redirect
limits and HTTPS downgrade rejection still apply. Cloning the client retains
the same connector. Debug output reports only that a connector is present.

## Connector and stream responsibilities

The connector owns resolution, destination policy and connection admission.
For a tunnel this includes IPv4/IPv6 support, loopback/LAN policy, captured
generations, cancellation and revocation. HTTP deliberately has no fallback
policy. The separate [native tunnel adapter](tunnel-http.md) now supplies a
generation-bound implementation; production torrent integration remains pending.
Every redirect is a new admission decision through the connector. Connector errors propagate
to the caller, so implementations must redact credentials and provider payloads
from their errors.

The connector must bound resolution and connection work by the supplied
deadline. The stream must honor each configured read/write timeout, including
flushes, and reject I/O after its transport is revoked. HTTP resets each timeout
to the remaining operation budget and refuses further I/O when it expires.
It cannot preempt a connector or stream implementation that blocks beyond its
contract. A connector returning after its deadline cannot send an HTTP request
or TLS handshake through the resulting stream.

Without `with_connector`, the existing direct client behavior remains: host
resolution, physical TCP and environment proxy support. Its host resolver is
still a blocking standard-library lookup.

## Delivery boundary

This delivers [#283](https://github.com/alecerf/mynou/issues/283), a prerequisite
of [#221](https://github.com/alecerf/mynou/issues/221). Production torrent peers,
listeners, discovery, metadata inspection and their orchestration callers still
need the explicit transport context and network-inventory gate. This API does
not activate VPN protection. Profiles/lifecycle #222, isolation and resource
evidence #224, and parent #111 remain open.

Native fixtures cover in-memory HTTP with unresolved synthetic names, redirects
and credentials, disabled proxies, connector rejection without physical
fallback, late connection results, response limits, and TLS ClientHello on a
supplied stream. A fragmented custom stream also exercises authenticated TLS,
application data, key updates and rejection of invalid authentication, names and
trust anchors. End-to-end HTTPS verifies the URL hostname and rejects a redirect
to plaintext HTTP. These fixtures validate the connector seam, not torrent
isolation or interoperability with a VPN server.

The [native tunnel HTTP driver](tunnel-http.md), delivered separately by #286,
implements this seam for virtual TCP and DNS. It does not complete #221.
