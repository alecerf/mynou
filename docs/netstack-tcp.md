# Offline virtual TCP client, listener and bounded flow table

[#205](https://github.com/alecerf/mynou/issues/205) adds the TCP layer of the
userspace IP stack described in [the design](/docs/wireguard-design.md), on top of
the strict inner IPv4 codec of [the IPv4/UDP/DNS guide](/docs/netstack.md). It only
sees inner IP datagrams: what `Session::receive` of [the transport](/docs/wireguard-transport.md)
returns goes in through `TcpStack::ingest`, and what `TcpStack::poll` returns is
what a driver would encrypt and send. There is no socket, tunnel coupling, engine,
profile, DNS-over-TCP or torrent caller, so **this is not routing, host networking
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
`Closed`, and `Failure` is `Reset`, `Refused` or `Timeout`.
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
  the flow fails with `Timeout` and an RST after `retries` tries. There is no RTT
  estimation, no delayed or selective acknowledgment, no Nagle and no timestamps:
  the retransmission timeout starts at `initial_rto` and only backs off.
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
| Retransmission timeout | 1 s to 30 s | backoff stops at the cap |
| TIME-WAIT, closed linger, zero-window stall | 30 s, 30 s, 120 s | the flow is released or fails |

`TcpConfig::new` takes the same `Ipv4Config` as the UDP layer and an MTU, validates
every bound and rejects values outside the documented ranges. The initial sequence
number and ephemeral port come from the injectable `Entropy`; if it fails the
operation fails and no predictable value is used.

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
is involved. Throughput, congestion behavior against real paths and interoperability
are not measured or claimed.

## Remaining obligations

The driver that feeds decrypted session bytes in, encrypts what comes out, owns
the clock and generations and revokes the UDP table, the DNS client and this stack
together is now [the tunnel runtime](/docs/tunnel.md). Still missing: IP fragmentation and reassembly, ICMP and path-MTU handling; window
scaling, selective acknowledgment, RTT estimation, simultaneous open and a TCP
Keep-Alive if torrent peers need them; DNS over this TCP for truncated answers; the
transport, HTTP/TLS and torrent integration; profile controls; an independently
implemented peer in CI; and the isolation evidence listed in
[the design](/docs/wireguard-design.md). Nothing here changes user-visible behavior.
