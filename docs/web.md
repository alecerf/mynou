# Browser management

Open **http://127.0.0.1:8787/ui** after starting the service or its generated
Docker installation. The browser interface shares the existing HTTP listener.
Sign in with `MYNOU_API_TOKEN` from the installation's private `.env`.

All pages are rendered by Rust. Forms work without JavaScript, external fonts,
CDNs, browser packages, or a build pipeline for frontend dependencies. Refresh
the page to see the latest progress. The embedded stylesheet includes a narrow
screen layout, visible keyboard focus, labelled fields, table headers and a
skip-to-content link. Browser and assistive-technology interoperability has not
been independently reviewed.

## Everyday operations

| Page | Operations |
| --- | --- |
| Overview | Request/transfer counts, integration errors, Plex watchlist synchronization |
| Jobs | Title/identifier and state filters, progress, details, recent history, cancel and retry |
| Library | Current owned imports, monitoring, missing files/baselines, pending upgrades |
| Search | Preview movie/episode selection with reasons; record movie, episode, series or file requests |
| Series | Track scopes with optional acquisition, monitoring/specials, earliest air date, episode choices, catalog refresh, explicit mapped packs, guarded automatic pack previews, reviewed numbering choices and shared video ownership |
| Calendar | Filter known episode dates by window and series, inspect monitoring and request states |
| Transfers | Native queue, durable pause/resume, queue/file priorities, payload counters and seeding policies |

Search previews contact configured sources without recording jobs. They retain
the entered title and numeric identity in the form. Recording the request runs
automatic selection later; the displayed candidate is not reserved. Series
requests now retain a catalog plan and monitoring record, even when no episode
has aired. See [series monitoring](series.md) for acquisition rules and limits.
Automatic previews support movies and individual episodes. An explicit URL or server path bypasses
automatic source selection, and that source input is cleared after preview.
Source paths refer to files visible to the service, including container mounts;
this page does not upload a file from the browser.

Owned job details expose monitoring controls and, for an import missing one, a
one-time matching release baseline. Preview upgrades before choosing **Apply
available upgrades**. Apply performs a fresh search pass; it does not apply an
immutable preview snapshot. Upgrades retain earlier imports until a replacement
is ready and retain old files afterward. See [library behavior](library.md).

Transfer pauses affect every request sharing that torrent. File priorities
order required pieces. File details distinguish selected and verified paths;
**Include file** and **Download all files** expand retained interests without
clearing a user pause. A partial transfer cannot seed. Rates use bytes per second,
and zero means unlimited. A ratio of `1000` means 1:1; blank ratio/time fields
mean no local seeding cap. Saving a policy replaces seeding defaults in full.
Restoring defaults removes the override. Global bandwidth caps always remain
mandatory. See [transfer controls](transfers.md).

Pack forms accept an exact file path for each catalog episode in a JSON array.
Create an unmonitored series scope when choosing a pack before individual
automatic acquisition. Existing requests are reused, and missing paths never
fall back to another video. Job details show mappings protected from worker changes. New native pack transfers need only their mapped files and necessary verified
boundary pieces. Earlier full transfers keep their acquisition policy. See [pack operations](packs.md) for format, validation
and storage-version limits.

Series details also provide **Preview season packs**. A resolved report displays
catalog episodes, exact file mappings and bounded metadata decisions; its
**Acquire resolved pack** form carries both scope and candidate guards. Apply
searches again and rejects changed catalog/request scopes, source hashes or
mappings. Preview records no jobs and requests no torrent payload. On-demand
acquisition does not enable background monitoring. See
[automatic packs](automatic-packs.md) for filename rules, bounds and persistence.

## Bulk changes and bounds

Select at most 32 entries on one page. Jobs support cancel/retry, library entries
and series records support monitor/unmonitor, and transfers support pause/resume.
All identifiers, duplicates and the requested operation are checked before changes start. Each
valid identifier is then handled independently. An unknown or ineligible entry
does not prevent other entries from succeeding. The following page reports each
result and the count of successful actions. A bulk change is not an atomic
transaction. Cancellation retains downloaded sources and library imports.

Lists contain at most 50 rows per page. Detail file and event lists are also
paginated, including series episode plans. Calendar windows contain at most
367 days and use catalog dates against UTC, with no invented dates for undated
episodes. Filters remain attached to pagination links. Search/upgrade reports
keep the existing source/report bounds. Browser forms accept at most 64 fields,
65,536 encoded bytes and 8,192 decoded bytes per field; source input is therefore
smaller than the API's maximum. Malformed escapes, invalid UTF-8, unexpected
fields, duplicate scalar fields, controls and invalid numbers are rejected.

## Sign-in and deployment

The API token is submitted in a POST body. It never becomes a URL, cookie, HTML
value, browser storage entry, or session-store value. Browser authentication uses
independently generated 256-bit session and form tokens. Successful sign-in
rotates both, preventing the anonymous sign-in cookie from becoming an
authenticated session. Cookies are `HttpOnly`, `SameSite=Strict` and scoped to
`/ui`. HTTPS sign-in adds `Secure` to the authenticated cookie.

Sessions live only in memory and expire eight hours after sign-in; page reads do
not extend that deadline. Sign-out removes the session, and service restart ends
all sessions. Anonymous sign-in challenges expire after ten minutes and are
removed after five incorrect token attempts. At most 128 sessions/challenges
exist; new anonymous challenges can evict an older anonymous challenge, but do
not evict authenticated sessions.

Every form action requires its session cookie, matching form token, and a
same-origin `Origin` header. Cross-site fetch metadata is rejected. Sessions are
bound to the requested host/port; authenticated actions remain bound to the
HTTP/HTTPS origin used at sign-in. Forwarded-host/protocol headers are not used.
No user-supplied return URL is accepted. Browser responses prevent caching,
referrer propagation and framing; a content security policy disables scripts
and restricts styles/forms to the same origin. Dynamic display labels are
escaped, bounded and subject to the public report credential redaction rules.

Generated Docker installations publish management on loopback. For remote
browser access, use a TLS reverse proxy that preserves the browser's **Host** and
**Origin**, forwards `/ui` without rewriting its prefix, and keeps the plain HTTP
listener private. Sign in through the HTTPS address to obtain a Secure session
cookie. The anonymous challenge is not an authenticated cookie and does not
have Secure before the POST establishes the browser origin. Changing address
or scheme requires a new sign-in.

This interface has the privileges of the shared API token. It does not add
individual users, approval policies or quotas. API requests still require
`Authorization: Bearer …`; browser cookies never authenticate `/api` routes,
and a Bearer header does not sign a browser session in. Configuration editing,
notifications, WebSocket updates and personal-installation validation remain
outside this release.

[Docker installation](deployment.md) · [Explicit limits](limits.md) ·
[CI validation](validation.md) · [Release roadmap](roadmap.md)

Episode numbering in series details previews catalog/source choices before the
guarded **Save reviewed numbering** action. See [numbering](numbering.md) for
identity preservation, larger CLI/API decisions and exact bounds.

**One video for multiple episodes** previews authenticated metadata and the full
canonical owner range before **Record reviewed shared ownership**. Source
credentials stay server-side in one ten-minute review per session. Group import
and exact Plex path confirmation are shared; each episode retains its own job
state. Individual remapping and upgrades are blocked for shared owners. See
[shared files](shared-files.md) for reuse, cancellation and persistence rules.

Current shared-owner job pages expose **Preview whole-group baseline** and
**Preview whole-group replacement**. Review the complete scope, release and exact
authenticated path before **Record reviewed whole-group decision**. One private
ten-minute review is retained per session across shared-file and group actions.
The apply form contains only owner/guard/CSRF fields. Staged replacement owners
wait for the remaining confirmations; cancel/retry affects their entire group.
See [group upgrades](group-upgrades.md).
