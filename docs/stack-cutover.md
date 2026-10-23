# Qualify a Plex-plus-Mynou installation

This is the operator protocol for [#344](https://github.com/alecerf/mynou/issues/344),
the final gate for [#335](https://github.com/alecerf/mynou/issues/335).
Implementation delivery and synthetic fixtures do not pass this gate. Keep both
issues open until the private evidence below is complete. Plex remains in use.

## Prepare without interrupting the stack

Keep a mode-0700 private preparation directory outside version control; keep
configuration, manifests and reports mode 0600. Never attach raw configuration,
service environments, account identities, paths or media inventories to GitHub.
Record aliases and aggregate outcomes in the public report.

Maintain a private manifest with one row per currently used function, source,
account, title profile, library root and torrent. Each row needs its legacy
reference, intended native configuration/control, protection policy, validation
evidence and unresolved blockers. An empty inventory is a blocker, not evidence
that the installation needs no migration. Use native library/torrent controls
for an external migration; Mynou has no other-manager importer. Preserve original
files, seed payloads and shared-file relationships.

Record the exact release tag, source commit, asset name and verified checksum.
The release must contain all accepted implementation changes, including #355.
A local build from that commit is useful preparation but is not a released
installation. Its displayed package version alone cannot establish provenance.
Follow [release verification](validation.md#verify-release-assets); do not
replace an existing release asset or change the version in a feature PR.

Prepare configuration and credentials privately. Disable downloads and all
automation until the manifest and protection policies have been reviewed.
Protect each formerly protected indexer and IRC source explicitly as described
in [protected sources](protected-sources.md); torrent VPN alone is insufficient.
Do not run `doctor`, previews or source commands against real sources as though
they were offline checks: they can make network requests.

Choose stable executable/configuration paths and existing media/download/state
roots. Follow [service permissions and volume requirements](macos-service.md).
Documents is subject to macOS privacy controls; a fixture in `/private/tmp` does
not establish launchd access to an installation in Documents. Never create an
empty directory over a missing volume or enable a second writer.

Before requesting the operator window, fill in the private manifest, release
provenance, proposed migration actions, storage/space estimates, backup locations,
exact service identities, test data, duration and rollback steps. Unknown values
remain blockers. A plan or issue assignment does not authorize service stops.

## Approve the operator window

Obtain explicit owner authorization for the scheduled stop/migration/trial,
using the concrete private plan. Propose a 24-hour observation window, including
account logout/login constraints and the expected future-episode observation;
the owner must accept or adjust it before the trial. Record start/end times,
disk-space baseline, allowed data changes and who can trigger rollback.

Stop conditions are missing protection, an unexpected source request, a second
writer, missing volumes, unexpected acquisition/rename/deletion, broken seed
payloads, failed Plex visibility, unbounded disk growth, unrecoverable pending
work, or a failed backup/restore check. On a stop condition, pause the trial and
apply the reviewed rollback; do not delete data to make acceptance pass.

## Execute and record privately

1. Stop the exact nine legacy automation services in the approved plan: Radarr,
   Sonarr, Prowlarr, Pulsarr, qBittorrent, Gluetun, Autobrr, Qui and the legacy
   homepage. Keep Plex available. Preserve containers, configurations, volumes
   and data. Confirm writers have exited before making coherent backups or
   letting Mynou own those files. Retain the previous binary and compatible
   state; do not claim a live filesystem copy is a coherent state snapshot.
2. Verify backup restoration in separate directories, with payload and media
   preservation. Install the verified release and migrate through native
   controls, following the private manifest. Verify imported files and retained
   seeds before enabling scheduling. Record intentional naming/retention
   differences and obtain acceptance of each difference.
3. Verify every source through Mynou's protected native WireGuard runtime:
   one bounded search and one feed read per source, spaced requests and no
   automatic retries, plus one resolved bracketed IRC announcement. Keep
   source-specific data private; publish one alias-only result per source.
   Ensure background polling cannot add unintended probe requests.
4. Exercise every authorized watchlist account, present-title deduplication,
   absent-title acquisition, film and episode imports, future-episode monitoring,
   per-title quality cutoffs/upgrades, supported packs, wanted-only feed/IRC
   routing and authenticated private metainfo retrieval. Verify expected Plex
   visibility, periodic/completion cross-seeding and shared-data preservation.
5. Use authorized test data to verify protected transfer, NAT-PMP renewal and
   externally observed inbound seed reachability with the supported provider.
   Record reconnect/restart behavior and provider limits. Synthetic tunnel
   fixtures and an allocated port alone do not prove external reachability.
6. Verify unattended restart, missing-volume diagnostics, pending work and
   retained profiles/seeds. Observe the approved load and disk-space baseline
   throughout the agreed window. Rehearse rollback without overwriting newer
   media/payloads; restore matching state and binary together when needed.

## Decide and close

The private report must map every #344 acceptance item to timestamped evidence,
an explicit accepted difference, or a blocker. Publish only release/commit
provenance, aliases, aggregate results, limitations and the go/no-go decision.
Close #344 only after the approved trial and every required item passes; then
reconcile and close #335. A failed or unperformed trial leaves both open.

Cleanup is a separate decision after acceptance. Offer an exact removal plan
only after successful qualification, and obtain separate authorization before
deleting obsolete services/configuration. Exclude media, downloads, required
state, secrets and rollback backups.
