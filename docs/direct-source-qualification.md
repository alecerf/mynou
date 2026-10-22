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
| Interface | Authorized provider documentation or sanitized response fixtures; distinguish the native endpoint from an adapter endpoint |
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
