# Release selection profiles

Profiles decide which release Mynou downloads when it searches your sources.
Movie and episode requests can use different named profiles. A profile first
rejects unsuitable candidates, then ranks the rest. Library upgrades use the
same profiles to compare a recorded release with new candidates; see
[library upgrades](library.md).

## Configure profiles

Add an optional `selection` object to `mynou.json`:

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
3. more seeds, then a deterministic tie-breaker.

With an unrestricted profile and no score rules, seed count decides. Source
order never bypasses a profile, and when nothing is accepted Mynou does not
fall back to a rejected release. Strict title, year and episode matching still
applies: a profile cannot turn an unrelated release into a match.

A request with an explicit source (`submit --url` or `submit --path`) is your
own choice: profiles apply only to automatic searches.

## Preview a search

```sh
mynou search --title "Example Movie" --year 2026 --config ./mynou.json
mynou search --title "Example Series" --kind episode --season 1 --episode 2 \
  --config ./mynou.json
```

A preview contacts your sources but records no request, writes nothing to the
journal and starts no download. The report lists accepted and rejected
candidates with their attributes, ranks and reasons, opaque candidate IDs and
`selected_candidate_id` for the proposed winner. It never includes acquisition
URLs or credentials. `candidate_count`, `reported_count` and `truncated` show
whether every row is displayed; the winner is always chosen from the full set.
Per-source counts show configured, successful and failed responses, and a
partially failed search can still choose from the sources that answered.

The API equivalent is `POST /api/search` with the same object as
`POST /api/jobs`, for example
`{"kind": "episode", "title": "Example Series", "year": 2026, "season": 1, "episode": 2}`.
A body containing `source_url` or `source_path` returns `manual_override: true`
without searching or echoing the source. `GET /api/profiles` returns the
configured profiles, and `mynou doctor` shows the active profile names. The
browser's **Search** page offers the same preview.

If your sources name releases differently, a restrictive profile may reject
them: inspect the preview and adjust the policy deliberately. Keep source
credentials in environment variables, never in profile terms. Bounds are listed
in [limits](limits.md#search-and-selection).
