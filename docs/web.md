# Browser interface

Open **http://127.0.0.1:8787/ui** after starting the service and sign in with
`MYNOU_API_TOKEN` from the installation's private `.env`. The interface shares
the API's listener and has the same privileges as the API token: there are no
individual users.

Pages are rendered by Mynou itself and work without JavaScript, external fonts,
CDNs or frontend packages. They include a narrow-screen layout, visible keyboard
focus, labelled fields, table headers and a skip-to-content link. Browser and
assistive-technology compatibility has not been independently reviewed.

## Pages

| Page | What you can do |
| --- | --- |
| Overview | Request and transfer counts, integration errors, Plex watchlist synchronization |
| Setup | Configuration checklist and first-request guidance (see below) |
| Jobs | Filter by title, ID or state; progress, details, history, cancel and retry |
| Library | Owned imports, monitoring, missing files or baselines, pending upgrades |
| Search | Preview movie or episode selection with reasons; record a movie, episode, series or file request |
| Series | Track series, monitoring, specials, earliest date, episode exclusions, catalog refresh, mapped and automatic packs, numbering and shared videos |
| Calendar | Known episode dates by window and series, with monitoring and request states |
| Transfers | Queue, pause and resume, queue and file priorities, file selection, counters and seeding policies |
| Requesters | Plex requester accounts, polls, demand and reviewed controls |
| Announcements | IRC source health, rules, history and reviews |
| Indexers | Source health and reviewed controls |

A search preview contacts your sources but records nothing; recording the
request runs automatic selection later, so the displayed candidate is not
reserved. An explicit URL or server path skips automatic selection and is
cleared from the form after preview. Paths refer to files the service can see;
the browser does not upload files. Applying
upgrades runs a fresh search rather than replaying the preview.

Shared videos, group upgrades and requester, indexer and announcement controls
work in two steps: a preview, then an apply form that carries only the reviewed
guard. The session keeps one pending review for ten minutes; a new review of any
kind replaces it, and sign-out or a restart discards it. Episode numbering also
asks for a preview before **Save reviewed numbering**.

The guides describe each operation: [selection](selection.md),
[library](library.md), [series](series.md), [packs](packs.md),
[transfers](transfers.md), [requesters](requesters.md) and
[sources](sources.md).

## Guided setup

**Setup** (or **Check setup** on Overview) lists the loaded folder settings,
active sources and their authentication, the download route, optional catalog
credentials and Plex settings. Missing credentials, malformed addresses,
disabled sources and incompatible settings get a fixed explanation and a fix.
"Configured" means a setting and its credential are present; it does not prove
that authentication, connectivity or storage work. The page never shows paths,
source addresses, credential names or values, or upstream errors, and opening it
performs no network request or write.

Mynou does not edit its configuration from the browser. Change `mynou.json` and
`.env`, restart the service and run `mynou doctor`; see
[Configure Mynou](macos.md#configure-mynou). Then preview a request on
**Search**, record it when ready and follow it in **Jobs**.

## Live progress

Jobs and Transfers lists and details have **Enable live updates**, **Pause
updates** and **Refresh page** controls. Updates start paused on every page and
the choice is not remembered. While enabled and visible, the page asks for its
displayed rows every ten seconds, one request at a time.

Only state, progress, attempts and transfer counters change in place; filters,
pagination, selected rows, focus and unfinished forms stay as they are. Refresh
the page to see new or removed rows, messages and new controls. A row that
disappears shows **Unavailable**. The freshness time is the last complete
update, not an estimate of speed or remaining time.

If the session expires, updates stop and **Sign in again** appears. A browser
that strips the same-origin `Referer` also shows it; use **Refresh page** there.
Other failures pause updates until **Retry live updates** or a page refresh;
there is no automatic retry loop.

## Bulk changes and lists

Select up to 32 entries on a page: jobs support cancel and retry, library
entries and series support monitor and unmonitor, and transfers support pause
and resume. Every entry is checked before anything changes, then each one is
handled on its own, so an invalid entry does not block the others. The next page
reports each result; a bulk change is not one transaction. Cancelling keeps
downloads and imports.

Lists show 50 rows per page. File lists, event histories and episode plans are
paginated too, and filters stay attached to page links.

## Sign-in, sessions and remote access

The token is sent once in a form body and never becomes a URL, cookie, page
value or stored session value. Sign-in creates new random session and form
tokens; cookies are `HttpOnly`, `SameSite=Strict`, scoped to `/ui` and `Secure`
over HTTPS. Sessions live in memory and end eight hours after sign-in, at
sign-out or when the service restarts. Browser cookies never authenticate `/api`
routes, and an API Bearer header never signs a browser in.

Every action requires the session cookie, its form token and a same-origin
request. `Origin` decides when present; otherwise an absolute same-origin
`Referer` must match the host and port. A session stays bound to the host, port
and scheme used at sign-in. Forwarded host or protocol headers are ignored.
Responses forbid caching and framing and send `Referrer-Policy: same-origin`.
The content security policy allows only same-origin styles, forms and
connections and the content-pinned live-progress script; inline scripts and
handlers are blocked. Displayed labels are escaped, bounded and redacted.

The default listener is loopback only. For remote access, put a TLS reverse
proxy in front that keeps the browser's `Host` and `Origin` (or same-origin
`Referer`), forwards `/ui` without rewriting it and keeps the plain HTTP
listener private. Sign in through the HTTPS address to get a `Secure` cookie;
changing the address or scheme requires a new sign-in.
