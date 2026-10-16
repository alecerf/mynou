# Offline virtual TCP client, listener and bounded flow table

[#205](https://github.com/alecerf/mynou/issues/205) adds the TCP layer of the
userspace IP stack described in [the design](/docs/wireguard-design.md), on top of
the strict inner IPv4 codec of [the IPv4/UDP/DNS guide](/docs/netstack.md). It only
sees inner IP datagrams: what `Session::receive` of [the transport](/docs/wireguard-transport.md)
returns goes in through `TcpStack::ingest`, and what `TcpStack::poll` returns is
what a driver would encrypt and send. There is no host socket, engine,
profile or torrent caller in this layer, so **this is not routing, host networking
or VPN protection, and it is not independent interoperability evidence**. Parent
#111 stays open. The code lives in `src/netstack/tcp.rs` (segments) and
`src/netstack/stream.rs` (flows): pure, std only, with no clock reads, threads or
I/O. The caller passes a captured generation and a monotonic elapsed time.

## Segment codec

`tcp::parse` accepts a segment only if the IP layer already accepted it as
protocol 6 for the configured address (`ipv4::parse_protocol`; the UDP entry point
still refuses TCP and the reverse). Then:

- the data offset is 5 to 15 words and the bytes exist; both ports are nonzero;
- the reserved bits and NS are zero, urgent, ECN-echo and CWR are refused, SYN with
  FIN or RST and FIN with RST are refused, and the urgent pointer is zero;
- the pseudo-header checksum verifies. It is mandatory over IPv4 and a computed
  zero is sent as zero (the UDP "all ones" rule does not apply);
- options are walked by length: End-of-Options stops the scan, No-Operation pads,
  every other option needs a length of at least 2 inside the option area and is
  skipped. A second MSS option, an MSS option that is not 4 bytes or an MSS below
  64 is refused; the MSS is only reported on a SYN.

The builder writes only a 4-byte MSS option on a SYN and fails with `TooLarge`
rather than fragmenting or truncating; `mtu` is between 576 and 1424 like the UDP
builder, so the MSS is `mtu - 40` (1384 at the session budget).

## Flows

`TcpStack` owns a flow table keyed by (local port, remote address, remote port)
and a listener table. Handles carry a serial, so a released flow or listener stays
dead after its port is reused. A different generation, a clock that moves backwards
or `revoke` irreversibly drops every flow, listener, buffered byte and queued
control packet; every later call returns `Revoked`.

| Call | Meaning |
| --- | --- |
| `connect` | Validates the remote (routable, not this host, inside the allowed network), draws an ephemeral port (49152 to 65535, at most 32 tries) and the initial sequence number from `Entropy`. The SYN leaves with the next `poll`. |
| `listen`, `accept`, `unlisten` | Bounded listeners. `accept` returns established flows; `unlisten` resets the flows nobody accepted. |
| `send`, `recv` | Bounded byte stream. `send` returns how many bytes fit (zero when full), `recv` returns data, `WouldBlock` or `Eof`; failures are explicit `Failed(reason)`. |
| `close`, `abort` | `close` shuts down sending (the FIN follows queued bytes) and releases a finished flow; `abort` sends an RST and releases at once. |
| `ingest`, `poll` | Segments in, segments and the next wake-up time out. |

State and failure are visible: `State` follows RFC 9293 (SYN-SENT to TIME-WAIT) plus
`Closed`, and `Failure` is `Reset`, `Refused`, `Timeout` or `Unreachable(code)`.
A closed or failed flow keeps its entry for `linger` so the caller sees the reason
and can drain received bytes, then it is released.

## Protocol behavior

- **Sequence arithmetic** is modulo 2^32 with serial-number comparison; tests start
  both sides next to the wrap point and transfer across it.
- **Acceptability** follows RFC 9293 section 3.10.7.4: a segment outside the receive
  window is dropped and acknowledged (never answered if it is an RST). An RST
  needs the exact next sequence number to end a flow; an in-window but inexact RST,
  and a SYN in a synchronized state, only draw an acknowledgment (RFC 5961). An
  acknowledgment of bytes that were never sent is refused.
- **Handshake**: client SYN, listener SYN-ACK, final ACK; SYN and SYN-ACK are
  retransmitted with doubling timeouts up to `syn_retries`. Simultaneous open is not
  supported. A SYN to a port without a listener, and any other segment for no flow,
  is answered with an RST as RFC 9293 prescribes, bounded by the `outbox` limit.
- **Data**: in-order bytes go to the receive buffer; out-of-order segments (at most
  `reorder_segments`, at most one receive buffer of bytes) are kept and merged
  when the gap fills; duplicates and overlaps are trimmed. A FIN that arrives
  ahead of missing bytes is ignored and comes again. The window is the free space
  of the receive buffer (no window scaling, 65535 at most); reading opens it and
  announces it when worthwhile.
- **Sending**: at most the smaller of the congestion window and the peer window.
  Loss is handled go-back-N from the first unacknowledged byte: three duplicate
  acknowledgments or a timeout set the threshold to half the flight (at least two
  segments) and restart slow start (Tahoe); the timeout doubles up to `max_rto` and
  the flow fails with `Timeout` and an RST after `retries` tries. RTT samples use
  the RFC 6298 smoothed mean and variance, with 1 ms granularity and a 1 s floor.
  Karn's rule suppresses samples until all retransmitted sequence space is
  acknowledged (including fast retransmit, PMTU resegmentation and probes).
  ACKs of ambiguous bytes preserve the backed-off timer; a fresh sample can
  reduce it. A retransmitted handshake starts data with at least 3 s RTO.
  There is no delayed or selective acknowledgment, Nagle or timestamps.
- **Zero window**: queued data waits and a one-byte probe is sent with backoff; the
  flow fails with `Timeout` if the window stays closed for `persist_limit`.
- **Closing**: FIN and the full RFC 9293 closing states, including simultaneous
  close; TIME-WAIT lasts `time_wait` and acknowledges a repeated FIN again.

## Bounds

| Bound | Default | Exhaustion |
| --- | --- | --- |
| Flows | 64 | `FlowsFull`; a SYN is dropped (`TableFull`) |
| Listeners | 8 | `ListenersFull` |
| Backlog per listener (half-open plus unaccepted) | 16 | SYN dropped (`Backlog`) |
| Send buffer per flow | 65536 | `send` takes fewer bytes |
| Receive buffer and window per flow | 65535 | data outside the window is dropped |
| Send, receive and out-of-order bytes in total | 4 MiB | `send` takes fewer bytes, data is dropped |
| Out-of-order segments per flow | 16 | the segment is dropped, the peer resends |
| Segments per flow per poll | 16 | `poll` reports an immediate wake-up |
| Queued control packets | 64 | the packet is dropped and counted |
| Retransmissions / SYN retransmissions | 8 / 5 | the flow fails with `Timeout` |
| Retransmission timeout | 1 s to 60 s | backoff stops at the cap |
| TIME-WAIT, closed linger, zero-window stall | 30 s, 30 s, 120 s | the flow is released or fails |

`TcpConfig::new` takes the same `Ipv4Config` as the UDP layer and an MTU, validates
every bound and rejects values outside the documented ranges, including an initial
RTO below 1 s or a maximum below 60 s. The initial sequence
number and ephemeral port come from the injectable `Entropy`; if it fails the
operation fails and no predictable value is used.

## ICMP and path MTU

`TcpStack::ingest_icmp` first applies the bounded checks in
[the IPv4 guide](/docs/netstack.md#icmp-advice-and-fragment-policy), then requires
an existing flow with the quoted local port, remote address and remote port.
The quoted TCP sequence must be within actually transmitted, unacknowledged
sequence space, including wraparound. An unsent SYN, acknowledged bytes, closed
flows and unknown tuples cannot be affected. This follows the sequence-checking
mitigation in [RFC 5927](https://www.rfc-editor.org/rfc/rfc5927#section-4.1);
it does not authenticate a router or defeat an observer or a correct sequence guess.

An eligible fragmentation-needed quote can only lower `path_mtu`, down to 576,
when the quoted datagram exceeded that MTU. The send MSS becomes the smaller of
the peer's MSS and the path's `mtu - 40`, and outstanding bytes are resegmented
from `snd_una` under the existing congestion, peer-window and retry limits.
Zero legacy MTUs and increases are ignored. No destination cache or new table is
created; the state is a few fixed fields within each bounded flow.

Other unreachable codes fail an outstanding active open with
`TcpFailure::Unreachable(code)`. Established connections retain their normal loss
recovery: `network_error` exposes advisory information without allowing ICMP to
reset the connection. `path_mtu` exposes the current per-flow budget. The tunnel
runtime dispatches ICMP through these same APIs and its generation fence.

## TCP option decision and measured limits

For #219, implement RTT estimation now; retain the unscaled window and cumulative
ACKs. Trackers and bounded DNS exchanges can use this stream, but **bulk torrent
throughput is not accepted as production-ready**. There is no torrent caller yet;
window scaling and SACK need a separate measured bulk-transfer implementation
before claiming good performance across high-delay or lossy paths. Simultaneous
open and TCP keep-alive also remain unsupported.

The deterministic warm-window test sends 2 MiB to grow congestion credit above
the advertised window, then queues 131070 bytes with a 1 MiB send buffer and a
64-segment burst. Exactly 65535 bytes can be in flight before acknowledgment.
At 100 ms RTT that window alone limits one stream to 655350 B/s (5.2428 Mbit/s),
before other overhead; this is an arithmetic ceiling from measured flight, not
an Internet throughput benchmark. The two-gap loss test loses segments one and
three of a four-MSS flight: seven MSS are emitted in total, including one
unnecessary retransmission of already-held data without SACK. Both are explicit
limitations, not evidence that scaling or SACK have no value.

The scripted RTT test measures a 400 ms handshake, then a 600 ms data round trip:
SRTT moves to 425 ms, variance to 200 ms and RTO to 1225 ms. Timeout doubles RTO
to 2450 ms; the ambiguous ACK leaves that backoff unchanged; a fresh 200 ms sample
moves it to 1221.875 ms. These exercise the estimator and Karn behavior, alongside
existing deterministic loss, reorder, corruption and zero-window cases.

## Evidence and limits

CI generates synthetic reference segments with Python's standard library, as for
[the UDP and DNS vectors](/docs/crypto-vectors.md): 14 IPv4/TCP packets with
checksums anchored to the same independent RFC 1071 implementation, a SYN and a
SYN-ACK with MSS, sequence wraparound, odd and maximal payloads, FIN and RST,
options the builder never writes (No-Operation padding, timestamps, window scale,
SACK-permitted, a stray MSS, End-of-Options) and a payload whose checksum is
computed as zero. Rust tests parse them and rebuild the canonical ones byte for
byte. They also cover every header field, every single-bit flip of a segment, the
option walker and the builder limits, and run two stacks against each other over a
scripted link: handshake, refusal and timeout, bulk transfer in both directions
(also across the sequence wrap), loss, reordering, duplication, fast retransmit,
zero window and its persist limit, give-up with an RST, graceful, simultaneous and
repeated closes, TIME-WAIT, backlog, flow-table, outbox and buffer exhaustion,
spoofed resets, SYNs, acknowledgments and out-of-window data, generation and
clock fences, and entropy failure.

The generator and the Rust code are by the same authors from the same RFCs, and
the state machine is only ever exercised against itself, so these tests catch
implementation slips, not misreadings shared by both, and no independent TCP stack
is involved. Only the deterministic flight/loss measurements above are claimed; real-path
throughput, congestion behavior and interoperability are not measured.

## Remaining obligations

The driver that feeds decrypted session bytes in, encrypts what comes out, owns
the clock and generations and revokes the UDP table, the DNS client and this stack
together is now [the tunnel runtime](/docs/tunnel.md). IP fragmentation is refused
and IPv6 remains blocked, as documented above. Still missing: window scaling,
selective acknowledgment, simultaneous open and TCP keep-alive for bulk peers; the
transport, HTTP/TLS and torrent integration; profile controls; an independently
implemented peer in CI; and the isolation evidence listed in
[the design](/docs/wireguard-design.md). Nothing here changes user-visible behavior.
