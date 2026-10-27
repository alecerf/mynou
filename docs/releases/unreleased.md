## Changed: Faster downloads and seeding from distant peers

Downloads keep many more requests in flight. Each peer can now have up to
512 blocks (8 MiB) and eight pieces outstanding, within the request queue it
advertises, through the VPN as well as directly; peers that advertise nothing
keep the previous 16-block window. A read or write queued for the VPN tunnel
could wait 2 ms for it, which made deep request queues slower than shallow
ones; while data flows, that wait is now 200 µs. Downloading a public Ubuntu
image through the VPN went from 9–31 to 37–52 MiB/s, with half the CPU per GiB.
On a local benchmark with 50 ms of added latency, a 64 MiB download from one
peer went from about 4 to 61 MiB/s, and seeding to libtorrent from about 4 to
40 MiB/s. Worker memory accounting includes every piece in flight, so torrents
with large pieces use fewer simultaneous peers. Seeding advertises the same
request queue, SHA-1 piece verification is about 85% faster, and unchanged
metadata or counter-only changes no longer rewrite durable files.

## Added: Downloads share their pieces and finish without waiting on slow peers

A download in progress now offers the pieces it has already verified to other
peers, and answers their metadata requests, when seeding is allowed. Upload
limits and the seed queue apply as before. Pieces are now fetched rarest
first among connected peers, within the order set by file priorities. Near
the end, a piece held by a peer that delivered no piece for two seconds is also
requested from an idle peer; the first verified copy cancels the other.

## Added: Encrypted peer connections

Mynou now speaks BitTorrent message stream encryption (MSE/PE). Peers that
connect encrypted are accepted instead of being refused, which made
libtorrent and qBittorrent wait about a second before falling back to plain
connections. When a peer closes a plain connection, Mynou retries it
encrypted. The encryption only matters for compatibility; privacy still comes
from the VPN.

## Fixed: Download limits keep requests flowing on distant peers

Setting a download speed limit no longer reduces every peer to one outstanding
block request. Each peer reserves a batch sized from its share of the global
and per-transfer limits over two observed round trips, then reads the replies
before waiting for more credit. Short scheduler delays no longer waste earned
credit. With a 16 MiB/s limit and 50 ms of latency, a 16 MiB download went
from about 60 seconds to under 3.

The [comparison and local measurements](/docs/torrent-benchmark.md) describe
the workloads, verification and remaining limitations.
