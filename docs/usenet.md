# Native Usenet stages

The active v0.22.0 increment provides original bounded NZB and yEnc format
primitives. Its source awaits its own complete CI. All code uses Rust std only.
These primitives establish format and accidental-corruption checks; full Usenet
search, NNTP acquisition, durable transfer management and recovery follow in
separate releases. No external decoder, downloader, archive tool or repair helper
is invoked.

## NZB inspection

```sh
mynou nzb-inspect original.nzb
```

Inspection reads a bounded regular local file without configuration, network
access, journal changes or payload downloads. The JSON report contains the
SHA-256 of the exact NZB bytes, file and segment counts, advertised byte total
and `content_verified: false`. Subjects, posters, groups and message IDs remain
private. A metadata digest authenticates those bytes alone; it does not establish
article contents or a film's identity.

The original XML parser is shared with RSS/Torznab. Input must be UTF-8 within
8 MiB, with at most 100,000 nodes, depth 64 and 64 attributes per element.
Comments, CDATA and standard/numeric entities are supported; DTDs, external
entities and internal processing instructions are rejected without fetching
anything. NZB accepts an unprefixed root and the optional standard
`http://www.newzbin.com/DTD/2003/nzb` default namespace. Unknown structures and
attributes fail instead of being inferred. A bounded optional head contains
meta entries; file entries contain exactly groups and segments.

At most 1,024 files and 32,768 total articles are accepted. Each file has 1–32
distinct ASCII newsgroups and contiguous, distinct segment numbers starting at
one. Message IDs must be bounded bare ASCII `local@domain` values, without angle
brackets, whitespace, quotes, backslashes or command delimiters. A message ID
cannot be reused across files. Advertised article sizes are positive and at
most 16 MiB each. These are untrusted metadata claims.

## yEnc and complete-file integrity

`usenet::yenc::decode` validates one article and returns an immutable verified
part. Headers are bounded UTF-8, payload remains binary, and line endings must be
CRLF. Header fields are explicit and distinct. A bounded preamble is allowed;
trailing payload or a second encoded block is rejected. Filenames are plain
bounded basenames, with no traversal, separators, stream suffixes, controls or
ambiguous trailing characters. Single parts require a matching whole-file CRC32.
Multipart articles require explicit part/total, inclusive byte range, matching
footer identity and matching part CRC32. The final part also requires the
whole-file CRC; earlier parts cannot claim it.

Decoded parts are limited to 16 MiB, with at most 32,768 declared parts and
one TiB of declared file size. The original CRC32 uses a constant lookup table
and supports incremental checks. CRC32 detects accidental corruption; it is not
cryptographic authentication and does not prove semantic media identity.

`usenet::yenc::assemble` accepts out-of-order verified parts, sorts by part number
and requires one consistent filename, total size and part count, exactly one of
every numbered part, exact contiguous byte coverage and the final whole-file
CRC. Overlaps, gaps, missing or conflicting parts fail. Its returned immutable
file contains only successfully verified bytes. This initial in-memory assembly
is limited to 64 MiB. Larger disk-backed assembly, verified restart recovery and
controlled import remain subsequent stages.

UUEncode, Base64/MIME article decoding, RAR/ZIP extraction and PAR2 repair are
outside this release's explicit format contract. NNTP and Newznab acquisition
are not yet connected. Original synthetic format fixtures cover corruption,
structure limits, escaped bytes, multipart ordering/coverage, CRC differences,
unsafe paths and the read-only CLI. Validation runs only in GitHub Actions.
