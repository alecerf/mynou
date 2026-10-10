## Reliable macOS demonstration

`mynou demo` no longer fails intermittently on macOS with `Plex: missing MediaContainer`. On macOS the demonstration's local Plex stand-in could read a request before its bytes arrived and answer with an empty success; it now waits for the complete request and rejects an incomplete one explicitly.

## Reliable API and peer connections on macOS

On macOS, API calls could fail intermittently with an incomplete-request error. Connections from incoming BitTorrent peers that were waiting for data could also keep a CPU core busy. Accepted connections now wait for their data with the usual timeouts.

## Torrent imports keep seeding

Importing a finished torrent into the library now copies the file instead of hard-linking it. The hard link gave the downloaded file a second name, and Mynou's torrent client then refused to read it, so seeding and the next restart check failed. Each imported movie or episode now uses its own disk space beside the seeded copy; imports from a local `source_path` are still hard-linked. Files already imported by an earlier version stay linked: delete the library copy and re-import, or remove the extra link, to let the torrent seed again.

## Browser interface removed

The browser interface (`/`, `/ui` and the sign-in, setup and live-progress pages) is removed. Mynou is now managed through the command line and the Bearer-token HTTP API, which are unchanged: `/api`, `/healthz` and `/readyz` keep working, and `mynou doctor` reports the configuration and service status. See [the macOS guide](../../macos.md).

## Usenet, PAR2 and archive processing removed

Mynou now acquires media through torrents only. Usenet downloads, Newznab indexers, NNTP providers, PAR2 inspection and repair, and ZIP/RAR inspection and extraction are removed, along with their commands and API routes.

Mynou rejects configuration fields it no longer knows. Before upgrading, delete the `usenet` section and every indexer whose `kind` is `newznab` from `mynou.json`.

## Requester approvals and quotas removed

Plex requester accounts no longer have manual approvals or active/daily request quotas. An enabled account admits its watchlist and IRC requests directly, and a reviewed policy change also applies to its requests that are not yet admitted. Opt-in, captured profiles and destinations, shared jobs, removal and retry are unchanged. See [requester policies](../../requesters.md).

A `requesters.bin` file saved by an earlier version can no longer be read, so Mynou does not start while that file exists. This affects only installations that have configured requester accounts. Back up your private storage before upgrading.

Existing requester records are not converted: demand that was pending approval, blocked by a quota or rejected is not preserved or specially handled, because the file holding it cannot be read. Ready media and shared transfers are not touched. Removing `requesters.bin` alone does not restore startup while requester-provenance jobs or committed IRC admissions remain, because startup requires their durable demand and origin. This change provides no migration, compatibility reader or recovery path.

API changes: the `approve` and `reject` requester control actions are removed, the `approved`, `approval_required`, `max_active` and `max_daily` fields are gone from requester policies and demand, the demand field `charged_at` is renamed `admitted_at`, and the IRC routing status `approval_required` is now `admission_required`.

## Outcome notifications removed

Mynou no longer records requester notification outcomes or delivers requester and IRC outcomes to HTTP endpoints. The `notifications`, `notifications-dispatch` and `notification-control` commands, the `/api/notifications` routes and the per-account notification preference are gone; request, transfer and announcement history is unchanged. There is no migration: Mynou does not start with a requester snapshot (`requesters.bin`) saved by an earlier version, with IRC history that held notification events, or with a configuration that has a `notifications` section.

## iCalendar export withdrawn

The private iCalendar (`.ics`) download, which never reached a release, is
withdrawn. Calendar views and scheduling are unchanged: `mynou calendar` and
`GET /api/calendar` still list known episode dates, and monitored series still
acquire newly aired episodes. See
[series and calendar](../../series.md).

## macOS Apple Silicon only

Mynou now ships a single executable, `mynou-vVERSION-macos-arm64` for Apple Silicon Macs, with a `SHA256SUMS` manifest. The Linux and Intel macOS executables and the Docker image are no longer built or published, the `setup-docker` command and the Docker guide are removed, and releases already published are unchanged. The default Plex address is now `http://127.0.0.1:32400` instead of `http://host.docker.internal:32400`: set `plex.url` if your Plex server is elsewhere, and see the [macOS guide](../../macos.md#configure-mynou) for the configuration reference.

## Interrupted imports no longer leave temporary files behind

If Mynou stops while copying a download into your library, the half-written `.mynou-*.tmp` file is now removed the next time an import writes to that folder.
