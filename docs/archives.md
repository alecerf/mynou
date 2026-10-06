# Native archive formats

The original ZIP and raw DEFLATE formats passed complete v0.22.7 CI. The active
v0.22.8 increment adds opt-in native Usenet library admission and requires its own
complete Actions validation. No external archive program or crate is used.

## Read-only inspection

```sh
mynou zip-inspect original.zip
```

This command reads a regular local file without configuration, network calls,
journal updates or output creation. It checks the ZIP structure and returns
entry paths, methods, declared sizes and expected CRCs. `content_verified` stays
false: header acceptance does not prove payload integrity or media identity.
Symbolic-link input paths are rejected.

## Supported ZIP subset

Classic single-disk ZIP with stored (method 0) or raw DEFLATE (method 8) entries
is supported. Local headers and the central directory must agree on names,
versions, methods, flags, timestamps, checksums and sizes. Signed and unsigned
data descriptors are checked against the directory; an unsigned descriptor
whose CRC equals the descriptor signature is intentionally unsupported.
Entry spans must account for the complete data area without overlap, padding,
self-extracting prefixes or hidden trailing data. An end-record comment is
allowed only when its stated length ends exactly at EOF.

Paths require ASCII or explicit valid UTF-8, at most 1,024 bytes, 32 components
and 255 bytes per component. Absolute paths, traversal, empty/dot components,
backslashes, colons, controls, trailing dots/spaces, Windows device names,
duplicates, case collisions and files used as ancestor directories are rejected.
Only DOS or Unix host metadata is supported. Links, devices, conflicting file
types and alternate-path/link extras are rejected. Names are metadata: the format
decoder never creates directories or chooses a filesystem destination.

ZIP64, split/multi-disk ZIP, encryption, legacy non-ASCII encodings, alternate
Unicode-path extras and other compression methods produce explicit errors.
RAR and PAR2 are separate future original implementations.

## Bounds and streaming

The source must be smaller than 4 GiB. The central directory is at most 2 MiB,
the end-record search at most 65,557 bytes and each extra-field area at most
4 KiB. At most 1,024 entries are accepted. Captured `archive::Limits` default to
1 GiB per decoded entry, 4 GiB total declared output, a 1,000:1 maximum declared
expansion ratio, and 16,384 DEFLATE blocks per entry. Caller-provided limits must
stay within the hard bounds: classic non-ZIP64 entry sizes, 1 TiB total output,
a 10,000:1 ratio and 65,536 blocks. Declared sizes are checked before decoding;
actual output is bounded independently and must match its exact declared size.

Raw DEFLATE uses a 32 KiB sliding window and 64 KiB input/output buffers, without
whole-payload allocation. Fixed, dynamic and stored blocks are decoded. Huffman
lookup tables contain at most 32,768 four-byte slots per alphabet; oversubscribed
or unsupported incomplete alphabets, reserved symbols, invalid distances,
truncated repeats and extra compressed data fail explicitly. Empty distance
alphabets are supported for literal-only blocks. Final-byte alignment bits are
ignored as permitted by RFC 1951; complete trailing bytes are rejected.

Cancellation is checked at blocks, bounded symbol/stored-byte intervals and
writes, including after the final write. Generic readers and writers must be
provided by a caller that bounds blocking I/O. These bounds are a resource
contract, not a throughput claim or a global disk quota.

## Payload API and publication

`Zip::extract` reparses and compares structural metadata before decoding a
selected entry into a caller-owned provisional `Write` sink. A successful result
requires exact decoded length and the expected CRC32 and returns a calculated
SHA-256. CRC detects accidental corruption; it does not authenticate media.
The API never selects a path or publishes a file. Any failure can leave partial
bytes in the provisional sink; callers must not publish them. Raw DEFLATE has
no embedded expected checksum, so its `Decoded` result contains calculated
checksums without a claim of an externally verified identity.

## Native ZIP library admission in 0.22.8

Enable native Usenet and a bound Newznab source, then opt in explicitly:

```json
{
  "usenet": {
    "downloads": {
      "enabled": true,
      "zip": {
        "max_entries": 1024,
        "max_entry_bytes": "1073741824",
        "max_total_bytes": "4294967296",
        "max_ratio": 1000,
        "max_blocks": 16384
      }
    }
  }
}
```

This is an excerpt to merge into configured provider settings. `zip: {}` uses
all defaults. Absent or null `zip` preserves direct-media behavior. Unknown keys,
zero limits and values above the format's hard bounds are rejected. Selection
freezes every limit; raising settings later cannot change an existing job.
Disabling ZIP withholds new authorization for a ZIP-aware job.

One NZB file containing one `.zip` is supported. The checked outer filename
must match the requested title/year, episode source numbering and captured
profile. Exactly one supported video entry (`mp4`, `m4v`, `mov`, `mkv`, `webm`,
`avi`) must be present, and its filename must satisfy the same gates. A second
video, including a sample, is rejected. Auxiliary entries are structurally checked
and counted against declared limits but never extracted. Both verified archive
size and decoded media size must satisfy the captured Newznab source policy.

The private journal records a typed intent before directories or payload writes:
owner and transfer, verified original source name/size/SHA, entry index/path/method/
size/CRC, and complete decoder limits. A private format-1 extraction frame has
writing or complete state. Fresh approved leases and bounded owner permission
gate extraction; hashing/decoding runs in one queue slot outside its mutex.
Permission renewal can proceed, while revocation and regrant invalidate the old
operation's result. Journal output attachment follows the final permission check.

Output stays in `archives/TRANSFER/output/LEAF` under the native queue state
root. Known writing intents may restart bounded partial bytes under fresh
permission; unverified bytes are never adopted. A complete exact frame/file proof
can join the preceding durable journal intent after interruption without
rewriting. Startup validates the namespace, source and retained output before
initializer writes, journal-tail repair or permission changes. Unknown, orphaned,
corrupt, rebound, symlinked or multiply linked data is preserved and rejected.
This is bounded crash recovery, not automatic cleanup or a global disk quota.

Exact decoded length and CRC, calculated SHA, source rehash and output rehash
precede a durable complete descriptor. ZIP-aware jobs require journal/snapshot
format 7; older direct-media records retain their existing identities and format
compatibility. Public origins expose enablement/prepared/verified booleans and
omit private extraction plans and proofs.

Native media analysis and cancellable independent-inode atomic library import
still follow extraction. Canonical movie/episode identity, approved requester,
captured destination and quality, and exact Plex imported-path confirmation stay
required. Format metadata and queue ownership cannot establish those approvals.
RAR, PAR2, ZIP64, multi-file/pack admission and Usenet upgrades remain separate
bounded increments.

## Original CI fixtures

Fixtures use only original synthetic bytes and the existing original demo MP4.
Independent raw fixed/dynamic streams were generated once as test data with
Python's standard-library encoder; Python and zlib are not Mynou dependencies.
The Rust fixture writer independently constructs stored ZIP headers/descriptors
and bit-level DEFLATE edge cases. GitHub Actions alone executes the tests and
validates exact bytes, CRC, SHA, wraparound, malformed inputs, captured bounds,
cancellation, sink errors, metadata changes and read-only CLI behavior.

The admission fixtures also cover private writing/completed interruption windows,
foreign or corrupt state, changed sources, links, format downgrade, stored/deflated
movie imports, canonical absolute-numbered episodes, captured limits across
restart, requester approval/destinations and exact Plex confirmation.
