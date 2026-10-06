# Native archive formats

The active 0.22.7 format implementation uses original Rust std-only ZIP and raw
DEFLATE code. Its own complete CI is pending. It is the format foundation for
subsequent ownership-bound native Usenet extraction; automatic archive import
is not enabled by this release. No external archive program or crate is used.

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

The next increment binds the verified source archive, selected entry, captured
limits and private output proof to canonical admission and current permission,
then runs the existing filename/profile/size/media/import/Plex gates. ZIP metadata
alone, raw queue ownership or a caller-supplied digest cannot bypass those gates.

## Original CI fixtures

Fixtures use only original synthetic bytes and the existing original demo MP4.
Independent raw fixed/dynamic streams were generated once as test data with
Python's standard-library encoder; Python and zlib are not Mynou dependencies.
The Rust fixture writer independently constructs stored ZIP headers/descriptors
and bit-level DEFLATE edge cases. GitHub Actions alone executes the tests and
validates exact bytes, CRC, SHA, wraparound, malformed inputs, captured bounds,
cancellation, sink errors, metadata changes and read-only CLI behavior.
