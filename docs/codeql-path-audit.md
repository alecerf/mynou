# Production path-injection review, 2026-10-10

Issue [#85](https://github.com/alecerf/mynou/issues/85) covers the 13 retained
production `rust/path-injection` alerts. This review uses trunk commit
`3cc4a57f1adb587abec09f7d44ca6eb617d2bb10`, CodeQL 2.27.2 analysis
`1929044552`, and its complete SARIF code flows. Line numbers below refer to
that commit. Test-only findings remain under #86. Previously closed findings
are historical, not classified by this review.

## Reported source and actual trust boundary

Every retained alert reports `TcpStream::connect_timeout` at `src/net.rs:327`
as its source. Each has four SARIF flows. The flows pass through the HTTP
response, `Source::parse_with_deadline` and restoration in
`Client::open_with_policy`, then through the returned `Client` as a whole.
For state sinks, the flow subsequently selects `self.config.state_dir`.
That field is not derived from the response: it is the caller's configuration,
stored in `Client` unchanged apart from the bound listen port. HTTP metadata
changes torrent metadata, hashes and trackers, never the configured roots.

The application constructs download roots from the operator's JSON
configuration (`src/config.rs:499–502`), not from API requests or torrent
metadata. The API authenticates before dispatching mutation/inspection routes
(`src/server.rs:209–216`). Explicit local torrent paths are a supported
operator input. Indexer JSON/RSS links pass `acquisition_url`, which accepts
HTTP(S) or magnets and resolves relative URL paths against the indexer origin;
it does not admit local filesystem paths.

## Per-alert conclusion

All 13 reported flows are **false positives**. Each row specifies the relevant
sink and why the reported network source cannot choose its path.

| Alert | Sink in `src/torrent.rs` | Evidence |
| --- | --- | --- |
| [15](https://github.com/alecerf/mynou/security/code-scanning/15) | 313, local torrent `File::open(source)` | The SARIF flows reach `listen_port()` and synthetic fixture magnets, then the local-file sink. Magnet parsing and HTTP loading are mutually exclusive branches of the local-file branch. Neither HTTP response bytes nor a fixture magnet can become this file path. An explicit operator-supplied local torrent path is intentionally supported. |
| [33](https://github.com/alecerf/mynou/security/code-scanning/33) | 362, `create_dir_all(path)` | The path is a configured root, a state-file parent under that root, or a data directory joined with a hex torrent ID. The HTTP response does not set these roots or provide a directory name. |
| [24](https://github.com/alecerf/mynou/security/code-scanning/24) | 363, directory `symlink_metadata(path)` | Same root provenance as #33; this inspection rejects a symlink/non-directory at the directory itself. |
| [36](https://github.com/alecerf/mynou/security/code-scanning/36) | 370, directory `set_permissions(path)` | Same root provenance as #33; permissions are set to 0700 after inspecting the directory itself. |
| [25](https://github.com/alecerf/mynou/security/code-scanning/25) | 387, state `symlink_metadata(path)` | `atomic_write` callers use fixed extensions on validated/computed hex torrent IDs under the configured state root. Existing destinations must be regular files, not symlinks or hardlinks. |
| [19](https://github.com/alecerf/mynou/security/code-scanning/19) | 413, temporary-file open | Parent is the configured state root; the basename contains only an internal process ID, timestamp and atomic counter. `create_new(true)` prevents following an existing temporary-file symlink; the file mode is 0600. |
| [29](https://github.com/alecerf/mynou/security/code-scanning/29) | 418, rename source | Source is the same exclusively created internal temporary filename as #19. No metadata filename enters it. |
| [31](https://github.com/alecerf/mynou/security/code-scanning/31) | 418, rename destination | Destination is a fixed-extension state filename under the configured root, as in #25. Rename publishes the temporary file, rather than writing through the destination. |
| [16](https://github.com/alecerf/mynou/security/code-scanning/16) | 419, parent-directory open | Parent is the configured state root; opening it is for directory synchronization. The network response does not set this field. |
| [22](https://github.com/alecerf/mynou/security/code-scanning/22) | 425, temporary-file cleanup | Removes only the internal temporary path from #19, on failure. The network response supplies neither parent nor basename. |
| [28](https://github.com/alecerf/mynou/security/code-scanning/28) | 1596, pause-marker inspection | Path is configured state root plus `Job.status.id` and `.paused`. The ID is computed from torrent hashes, or restored from a 40/64-character ASCII-hex filename and checked against its metadata/source hash. |
| [21](https://github.com/alecerf/mynou/security/code-scanning/21) | 1597, pause-marker removal | Same validated identity as #28. Public resume routes first resolve an existing job; a request ID cannot insert a new path. Removing a final symlink would remove the link itself, not its target. |
| [17](https://github.com/alecerf/mynou/security/code-scanning/17) | 1598, state-directory open | Uses only `config.state_dir` for synchronization after marker removal. The HTTP response cannot change it. |

## Identity, metadata and filesystem limits

`Source::id()` encodes fixed-size hashes as hexadecimal. Startup only restores
40/64-character ASCII-hex IDs and verifies their metadata/source identities.
`pause_persisted` independently checks the same filename alphabet and length.
The state writer's private-directory and regular-file checks complement those
path construction rules; they do not sanitize arbitrary untrusted paths.

Torrent payload names are a different input: `metainfo::component` rejects
empty names, `.`/`..`, `/`, `\`, NUL and `:`, and bounds each component to 255
bytes. `confined` permits only normal relative components and rejects existing
links/special files below the download base. The existing CI fixture
`symlink_download_and_symlink_metadata_are_rejected` covers payload and
persisted-metadata links. No new runtime defect was established by these 13
flows, so this audit adds no regression fixture or runtime change.

This is not a proof against a hostile operator configuration, a process with
the same filesystem privileges, or concurrent replacement of directories by a
local actor. `mkdir_private` checks its final directory, not every ancestor;
its metadata checks and later operations are not a race-free directory-handle
sandbox. The false-positive verdict rests on the absence of the reported
network-to-path data flow, not on claiming those broader properties. No local
tests, builds, scans or lint were run for this review.
