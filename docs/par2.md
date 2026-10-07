# Native PAR2 inspection in 0.22.11

`mynou par2-inspect FILE` reads one regular file containing a complete,
contiguous PAR2 core packet set and prints its checked metadata as JSON. It
does not open the described media files or create configuration, journals or
output files. Recovery bytes are hashed in bounded chunks rather than retained.

Supported core packets are Main, File Description, Input File Slice Checksum,
Recovery Slice and Creator. The reader verifies each packet MD5, the Main
recovery-set identifier, file identifiers derived from unpadded names, exact
metadata joins, checksum counts and recovery lengths. Identical duplicate packets
are accepted; conflicting duplicates, orphan metadata and mixed sets are rejected.
Main order is preserved. Offsets and SHA-256 digests describe the captured source
and recovery slices; they do not verify the contents of the described files.

MD5 is implemented originally from RFC 1321 solely for format compatibility.
It is not authentication and never grants permission to download, repair, import
or publish media. Packet integrity does not establish content or parity correctness.
The JSON explicitly reports `content_verified: false` and `repair_supported: false`.

This strict foundation requires a complete Main/Creator/description set in one
source and checksums for each recoverable file. Names and creator text are bounded
printable ASCII with exact zero padding; file names must pass portable relative
path checks and cannot alias by case or form file/ancestor conflicts. Unknown
standard packet types are rejected. Nonstandard packets are checksum-verified,
streamed within metadata bounds and counted as ignored. Unicode names, optional
standard semantics, damaged-packet resynchronization and volume merging are not
supported by this increment.

Default hard bounds are 256 MiB of source, 4,096 packets, 1,024 files, 32,768
input checksum slices, 1 MiB per slice, 8 MiB metadata and 64 MiB recovery bytes.
A declared file is limited to 4 GiB and total declared files to 64 GiB. Applications
can supply smaller validated `par2::Limits` and a cancellation flag through the
library API. Cancellation is checked between synchronous reads; it cannot interrupt
an arbitrary caller-provided blocking reader. Input length changes fail closed.
The file API rejects symbolic-link inputs and, on Unix, checks opened device/inode
identity against the captured metadata. Inspection is read-only, not a snapshot
guarantee for a file concurrently modified in place.

Original CI-only fixtures cover RFC vectors, packet corruption, semantic joins,
padding/identity, duplicates, paths, bounds, cancellation, streaming recovery
payloads and read-only CLI behavior. Actual verification is recorded in the linked
PR and GitHub Actions; this document does not claim unpublished test results.

The next bounded increment is original GF(2^16) recovery with independently
checked arithmetic and parity fixtures. Ownership-bound multi-file recovery,
volume merging and library admission require subsequent explicit work. Existing
Usenet ownership, requester, destination, media-analysis and Plex gates remain
required for any future admission.
