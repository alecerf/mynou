# Release selection profiles

Profiles decide which release Mynou downloads when it searches your sources.
Movie and episode requests can use different named profiles. A profile first
rejects unsuitable candidates, then ranks the rest. Library upgrades use the
same profiles to compare a recorded release with new candidates; see
[library upgrades](library.md).

## Configure profiles

Add an optional `selection` object to the settings document. The web interface
keeps it as it is; to change it, read the document from `GET /api/settings`
and send it back whole as `{"settings": {...}}` to `POST /api/settings`:

```json
{
  "selection": {
    "movie_profile": "hd",
    "episode_profile": "hd",
    "profiles": {
      "any": {},
      "hd": {
        "resolutions": [1080, 720],
        "cutoff_resolution": null,
        "sources": ["web-dl", "bluray", "webrip"],
        "codecs": ["h265", "h264"],
        "languages": ["en"],
        "allow_unknown_resolution": false,
        "allow_unknown_source": false,
        "allow_unknown_codec": false,
        "allow_unknown_language": false,
        "required_terms": [],
        "blocked_terms": ["cam"],
        "score_rules": [
          {"terms": ["proper"], "score": 10}
        ],
        "minimum_score": 0
      }
    }
  }
}
```

This example requires an explicitly identified 1080p or 720p release from the
listed sources, using H.265 or H.264 and an English audio marker. It prefers
1080p to 720p, WEB-DL to Blu-ray to WEBRip, and H.265 to H.264. A matching
`proper` rule adds ten points, which outweighs those ordered preferences.

Without `selection`, both request kinds use the unrestricted `any` profile.
`movie_profile` and `episode_profile` must name configured profiles. Restart
the service after changing the configuration.

| Attribute | Values, most preferred first |
| --- | --- |
| `resolutions` | `480`, `576`, `720`, `1080`, `2160` |
| `sources` | `cam`, `dvd`, `hdtv`, `webrip`, `web-dl`, `bluray`, `remux` |
| `codecs` | `h264`, `h265`, `av1`, `vp9` |
| `languages` | `en`, `fr`, `de`, `es`, `it`, `ja`, `ko`, `zh`, `pt`, `ru` |

Each list is ordered from most to least preferred. An empty list places no
restriction on that attribute. Unsupported values are a configuration error.
`cutoff_resolution` (default `null`) stops library upgrades once reached; see
[quality cutoffs](library.md#quality-cutoffs).

## Profiles for individual titles

A movie or a tracked series can use its own profile, for example `uhd` for a
few titles while the global profiles stay at `hd`. A binding names a configured
profile and the TMDB ID of the movie or series; a series binding applies to all
its episodes and season packs.

| Action | API |
| --- | --- |
| Bind | `POST /api/title-profiles/control` with `{"action": "set", "kind": "movie", "tmdb_id": 603, "profile": "uhd"}` |
| Unbind | The same with `{"action": "clear", "kind": "series", "tmdb_id": 1399}` |
| List | `GET /api/title-profiles?limit=N&after=CURSOR`, pages of up to 200 bindings in kind and TMDB order (100 by default) |

Every change is reviewed: without `"apply": true` the call previews it and
returns a `plan_id`; send the same body again with `"apply": true` and that
`"plan_id"` to save it. A plan expires when any binding or the configured
profiles change.

The profile used for a movie or an episode is, in order:

1. the binding of its movie or series;
2. the profile captured by the requester account that asked for it
   ([requesters](requesters.md));
3. the global `movie_profile` or `episode_profile`.

The binding applies to search previews, initial searches, monitored missing
episodes, feed and IRC routing, season-pack searches and library upgrades,
including cutoffs. A binding stores only the profile name: editing that profile
in the settings changes the bound titles too. If the profile is removed, the
title's searches fail with `unknown profile` until you rebind or clear it; Mynou
never falls back to the global profile. This also applies when a requester
captured an older definition under the same name; its captured destination is
preserved. Requests without a TMDB ID cannot be bound.

A change applies to work that has not selected a release yet and to later
upgrade checks. A release already selected, downloading or imported is kept,
and nothing is cancelled. A recorded release outside the new profile becomes
replaceable at the next upgrade check, like any release that no longer passes
its profile. Several requester accounts can share one job, as before; when its
title is bound, the binding decides for all of them.

## How a candidate is judged

Attributes come from the release title after the part that matches the
requested title and year or episode. Words in the media title itself, such as
`French` or `Remux`, are not treated as markers. These markers are claims in a
name: `VOSTFR` means French subtitles, not French audio, and `MULTI` does not
say which audio languages are present. Mynou never decodes the media before
choosing.

- When an attribute's list is not empty, a candidate without a recognized
  marker is rejected unless the matching `allow_unknown_*` flag is `true`.
  Conflicting markers and malformed or unsupported resolution markers are
  rejected even then. An unknown value never counts as a preferred one.
- For unrestricted attributes, marker problems appear in
  `assessment.attributes.issues` without rejecting the release.
- Terms match whole, case-insensitive token phrases in the release title, not
  substrings inside other words. Every `required_terms` entry must match; any
  matching `blocked_terms` entry rejects the candidate.
- A score rule applies when all of its `terms` match; matching rules add up.
  Totals below `minimum_score` are rejected. Its default of zero rejects any
  negative total, so set a lower minimum (for example `-100` for a `-10` rule)
  to downrank instead of reject.

Accepted candidates are ranked by:

1. the highest custom score;
2. resolution, source, codec and language preferences, in that order;
3. more seeds, then the smallest candidate identity digest for exact ties.

The digest tie-breaker is deterministic but can choose a different release
than earlier versions, which compared the title and URL on an exact tie.

With an unrestricted profile and no score rules, seed count decides. Source
order never bypasses a profile, and when nothing is accepted Mynou does not
fall back to a rejected release. Strict title, year and episode matching still
applies: a profile cannot turn an unrelated release into a match.

A request with an explicit source (`source_url` or `source_path`) is your
own choice: profiles apply only to automatic searches.

## Preview a search

`POST /api/search` (optionally `POST /api/search?limit=N&after=CURSOR`) takes
the same object as `POST /api/jobs`, for example
`{"kind": "episode", "title": "Example Series", "year": 2026, "season": 1, "episode": 2}`.
A preview contacts your sources but records no request, writes nothing to the
journal and starts no download. The report lists accepted and rejected
candidates with their attributes, ranks and reasons, opaque candidate IDs and
`selected_candidate_id` for the proposed winner. It never includes acquisition
URLs or credentials. Candidates are listed best first in pages of 100
(`limit` 1 to 200): `candidate_count` is the total, `reported_count` the rows
of this page and `next` an opaque cursor for the following page, passed back as
`after` (`null` on the last page). Each page repeats the live search,
so a candidate can move between pages if a source changed meanwhile; the winner
is always chosen from every candidate, not only the page.
Per-source counts show configured, successful and failed responses, and a
partially failed search can still choose from the sources that answered.

A body containing `source_url` or `source_path` returns `manual_override: true`
without searching or echoing the source. `GET /api/profiles` returns the
configured profiles.

## Choose a release by hand

The web interface opens this report from a job (**Choose a release**) and from
each episode of a followed series. Picking one release records it on the
request, and Mynou downloads it instead of the release it would have selected.
A rejected release can be chosen too, after a second confirmation; it is
recorded with the active profile, so later upgrades compare against it.

`POST /api/grab` takes `{"request": {...}, "candidate_id": "..."}`, the request
object as for `POST /api/search` and a candidate ID from its report. Mynou
searches again and accepts the candidate only if a source still offers it, so
the browser never sees an acquisition URL. A missing request is created; a
queued, failed or cancelled request takes the new release, and a transfer it
had is paused like on a retry. A request that is downloading, being processed
right now, already imported, or bound to its own source (`source_url`,
`source_path`, a season pack, IRC) is refused.

For an episode of a followed series, `POST /api/series/ID/search` with
`{"season": 1, "episode": 2}` (and optional `after` and `limit`) returns the
report together with the episode's `request`, and `POST /api/series/ID/grab`
with `{"season": 1, "episode": 2, "candidate_id": "..."}` chooses a release.
Both build the request as monitoring would, including any numbering decision.

If your sources name releases differently, a restrictive profile may reject
them: inspect the preview and adjust the policy deliberately. Keep source
credentials in environment variables, never in profile terms. Bounds are listed
in [limits](limits.md#search-and-selection).
