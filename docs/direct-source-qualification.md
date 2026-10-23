# Qualifying a direct source

Native RSS, Torznab and mapped JSON support does not establish compatibility
with a particular provider. A Prowlarr or Jackett endpoint proves only the
adapter contract. Replacing that adapter requires evidence from the source's
own supported interface.

Keep the source inventory, account identities, URLs containing credentials and
configuration values in a private manifest outside the repository. Public
results should use opaque aliases, synthetic examples and redacted diagnostics.

## Record the contract before implementing it

For each enabled source, record:

| Contract | Evidence needed |
| --- | --- |
| Interface | Provider documentation, request/response facts observed in public adapter definitions, or authorized sanitized fixtures; distinguish the native endpoint from an adapter endpoint |
| Search | Movie and individual-episode query syntax, supported identifiers, categories and expected matching results |
| Feed | Direct feed endpoint, ordering, entry identity and polling limits |
| Details | Release identity and catalog/hash fields needed by any configured IRC lookup |
| Metainfo | Authenticated torrent retrieval, origin restrictions and expected info hash |
| Authentication | Required method, renewal/expiry behavior and credential variable names; keep values private |
| Pagination | Page/cursor contract, termination and result bounds, or evidence that one page is sufficient |
| Limits | Rate limits, retry policy, response size and request deadlines |
| Protection | Whether the source must use `"protection": "protected"`, including authentication, DNS and downloads |

Compare the contract against [current source support](sources.md). In
particular, HTML scraping, interactive challenges, arbitrary authentication
headers and multiple result pages are not supplied by the existing JSON
mapping. Record unsupported requirements as blockers; do not guess endpoints,
copy an adapter implementation or call an adapter a direct source.

The public [Prowlarr indexer definitions](https://github.com/Prowlarr/Indexers)
are a useful starting point: they describe the underlying source requests and
response fields, including native Torznab or JSON APIs. Record the definition
path and exact commit in the private manifest. Cardigann use alone does not
imply HTML scraping or a need for browser cookies. Inspecting these protocol
facts does not establish deployed compatibility; use them to author native
configurations and independent synthetic fixtures, then validate the source.

## Validate with fixtures

Use independently authored synthetic or authorized sanitized fixtures. Cover
movie and episode results, empty and malformed responses, authentication
denial/expiry, rate limiting, any required pagination, and metainfo download
failure or hash mismatch. Exercise the configured native transport and verify
that diagnostics expose neither credentials nor raw response bodies.

Existing generic tests can support shared behavior, but each source still
needs evidence that its contract matches the chosen configuration. A probe
with a synthetic query alone does not prove matching discovery, feed behavior,
IRC details lookup or authenticated torrent retrieval.

## Record the result

Keep one row per source in the private manifest, with an opaque alias for the
public report. Include the tested commit, contract/fixture references, supported
operations, configuration method and an explicit pass/fail result with any
unresolved blocker. A source without enough evidence remains unqualified.

Record synthetic results separately from separately authorized authenticated
provider validation. Do not publish the private manifest or run live probes as
part of CI. Full replacement acceptance also needs the released binary and the
authorized operator trial described in issues #335 and #344; passing source
fixtures does not authorize a cutover.

## Synthetic contracts of the replacement manifest

The private manifest for issue #336 lists ten enabled sources. Their contracts
are held by `src/indexers/contract_tests.rs`, which publishes only aliases,
protocol families, parameter names, authentication placement and response
fields. Hosts, paths, accounts and credentials stay private.

| Family | Aliases | Configuration |
| --- | --- | --- |
| Bearer-authenticated JSON filter API | source-01, source-02, source-03 | `json` mapping with `tmdb_id`, season and episode roles, newest-first fixed parameters, `details`, `size`, `tmdb_id` and `info_hash` fields |
| JSON search API with a query key | source-07 | `json` mapping with `tmdb_id`, season and episode roles, a fixed page size and the same item fields |
| Native Torznab | source-04, source-05, source-06, source-08, source-09, source-10 | `torznab` source with a query key |

Each contract checks the movie search by catalog ID or title, the episode
search with season and episode, title filtering of returned releases, empty
and malformed answers, the newest-entries read used by feed watching and
cross-seed discovery, the cross-seed name search with size and hash, the
details lookup used by bracketed IRC announcements, the authenticated
same-origin `.torrent` download without redirects, denial without an
anonymous fallback and a `429` rest without another request. Credentials never
appear in reports.

Pagination: every source answers one page. The JSON sources request their
largest documented page (100 entries, newest first); per-title searches are
narrowed by catalog ID, season and episode, and feeds read only the newest
page. A Torznab source returns its own default page. A title with more matching
releases than one page would be truncated; no source requires another page for
the observed workflow.

| Alias | Synthetic contract | Live API read (2026-10-22) | Live Mynou runtime |
| --- | --- | --- | --- |
| source-01 | Pass | Pass | Pending authorization |
| source-02 | Pass | Pass | Pending authorization |
| source-03 | Pass | Pass | Pending authorization |
| source-04 | Pass | Pass | Pending authorization |
| source-05 | Pass | Pass | Pending authorization |
| source-06 | Pass | Pass | Pending authorization |
| source-07 | Pass | Pass | Pending authorization |
| source-08 | Pass | Pass | Pending authorization |
| source-09 | Pass | Pass | Pending authorization |
| source-10 | Pass | Pass | Pending authorization |

The live API reads used each source's own interface once, with its configured
credential, outside Mynou. A source qualifies only once the native runtime has
also searched it, read its feed and, for source-02, resolved an announcement,
in a separately authorized bounded run.
