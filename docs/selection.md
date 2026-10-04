# Release selection profiles

Mynou 0.7.0 introduces policies for choosing a release before automatic
acquisition. Movie and episode requests can use different named profiles.
Profiles filter candidates and rank those that remain. Since 0.8.0, the same
policies also compare an owned import's recorded baseline with proposed upgrade
candidates. See [library monitoring](library.md) for preview/apply commands,
per-entry policy and preservation of earlier files.

## Configure profiles

Add this optional object to `mynou.json` alongside `indexers`:

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
1080p to 720p, WEB-DL to Blu-ray to WEBRip, and H.265 to H.264. A matching `proper`
rule adds ten points and takes precedence over those ordered preferences.

If `selection` is absent, both request kinds use the unrestricted `any` profile.
An empty list places no restriction on that attribute. All `allow_unknown_*`
flags default to `false`; they affect an attribute only when its allowlist is
nonempty. Set a flag to `true` to permit missing or unrecognized markers for
that attribute. The flag does not permit conflicting recognized markers or an
explicitly malformed/unsupported resolution marker when that attribute is
restricted. An unknown attribute does not become a preferred known value.
For an unrestricted attribute, marker issues remain visible in
`assessment.attributes.issues` without rejecting an otherwise matching release.

Movie and episode profile names must refer to configured profiles. Use `any`
for one request kind and a restrictive profile for the other if desired.

## Attributes and title terms

Supported canonical profile values are:

| Attribute | Values |
| --- | --- |
| `resolutions` | `480`, `576`, `720`, `1080`, `2160` |
| `sources` | `cam`, `dvd`, `hdtv`, `webrip`, `web-dl`, `bluray`, `remux` |
| `codecs` | `h264`, `h265`, `av1`, `vp9` |
| `languages` | `en`, `fr`, `de`, `es`, `it`, `ja`, `ko`, `zh`, `pt`, `ru` |

Each allowlist is ordered from most preferred to least preferred. Unsupported
configuration values produce an explicit error instead of silently changing
the policy.

The optional `cutoff_resolution` defaults to `null`. A non-null value must appear
in the profile's `resolutions` list. For monitored upgrades, an accepted baseline
at that position or an earlier preferred position has reached the cutoff and
stops further upgrades. Preference order controls this rule, not numeric pixel
height; `[720, 1080]` with cutoff `1080` also stops at `720`. A reached cutoff
stops source, codec and score upgrades too. Initial candidate selection still
uses the normal ranking below.

Attributes are inferred from the release-title suffix after matching the
requested title and year or episode identity. A media title containing words
such as `French` or `Remux` must not itself become an audio or source marker.
An explicit `VOSTFR` marker describes French subtitles, not French audio;
`MULTI` leaves the actual audio languages unknown. These are release-name hints,
not an inspection of the downloaded media's tracks.

Terms also apply to the matched release-title suffix. They match contiguous
normalized token phrases. Matching is case insensitive and does not treat a
substring within an unrelated word as a match. All
`required_terms` must match. Any matching `blocked_terms` rejects a candidate.
A score rule applies only when every phrase in its `terms` matches; matching
rules add their signed scores together. `minimum_score` rejects candidates below
the configured threshold. `minimum_score` defaults to zero, so a negative total
is rejected by default. To use a negative rule for downranking rather than
rejection, set a sufficiently low `minimum_score`, such as `-100` for a
`-10` rule.

Lists are limited to 32 entries and each term to 128 bytes. Scores are bounded
to an absolute value of 100,000 per rule or minimum threshold. There may be
between 1 and 64 profiles; profile names contain 1 to 64 ASCII letters, digits,
hyphens or underscores. Configuration is validated when loaded.

## Ranking and acceptance

The existing strict movie/episode identity checks remain necessary. A profile
cannot turn an unrelated release or an unsupported season pack into a match.
Eligible candidates are evaluated in this order:

1. Reject required-term, blocked-term, attribute, or minimum-score violations.
2. Prefer the highest custom score.
3. Compare resolution, source, codec, and language preferences in that order.
4. Prefer more seeds, with deterministic final tie breaking.

With an unrestricted profile and no scoring rules, seed count remains the
primary preference. Source endpoints and indexer order do not bypass profile
requirements. If no candidate passes, Mynou does not silently use a rejected
release.

Explicit `submit --url` and local `submit --path` requests represent a source
you chose yourself. Profiles govern automatic source search; they do not
reinterpret those explicit inputs as a new search.

## Preview a search

The CLI accepts a movie or individual episode request:

```sh
mynou search --title "Example Movie" --kind movie --year 2026 \
  --config ./mynou.json
mynou search --title "Example Series" --kind episode --season 1 --episode 2 \
  --config ./mynou.json
```

The authenticated `GET /api/profiles` operation returns the configured profile
names and policies. `doctor` also displays the active movie and episode profile
names.

The equivalent authenticated search operation is `POST /api/search`, using the
same request object as `POST /api/jobs`:

```json
{
  "kind": "episode",
  "title": "Example Series",
  "year": 2026,
  "season": 1,
  "episode": 2
}
```

The JSON report includes accepted and rejected candidate assessments, inferred
attributes, ranks and decision reasons, opaque candidate IDs, and a
`selected_candidate_id` identifying the proposed winner when one exists.
Acquisition URLs and credentials are not included. A preview contacts the
configured sources but does not submit a job, write the request journal, or
start a torrent. Movie and episode previews share the same selection policy
used by automatic acquisition.

Reports include `candidate_count`, `reported_count` and `truncated`. At most
1,000 candidate rows are displayed, after sorting; a truncated report still
identifies the winner from the full evaluated set. Retained candidate data has
a 16 MiB budget, and exceeding it produces an explicit error. Indexer counts
show configured, successful and failed responses; a partially failed search
can still select from successful sources. Search has a 90-second budget for
HTTP/socket operations and processing checks. Standard-library synchronous DNS
may block longer; late results are rejected, so a DNS stall can exceed the
nominal wall-clock budget.

An API request containing `source_url` or `source_path` returns
`manual_override: true` with no candidate selection, without echoing the value
or contacting indexers. It does not download or validate the contents of that
explicit source. The CLI search command accepts identity fields only; use
`submit` to choose an explicit source.

Indexer credentials still belong in environment variables. Do not embed them
in profile terms. Candidates with missing language or quality markers may be
rejected by a restrictive profile; inspect the preview and adjust the policy
deliberately if your sources use different naming conventions.

## Profiles and upgrades

0.8.0 adds controlled upgrades for monitored owned imports with recorded release
baselines. Both baseline and candidate are assessed using the current configured
profile, even when the baseline was acquired under a different profile name.
Profile acceptance comes before quality rank. A baseline outside the current
profile can be replaced by an accepted candidate even with a lower raw rank;
changing a required language is one example of this deliberate policy change.
If the baseline is accepted, the candidate must have a strictly better custom
score or ordered attribute preferences. More seeds or a tie-breaking title alone
do not constitute an improvement.

Previews contact indexers but do not write journal state or queue a child.
Applying a decision queues a deduplicated replacement request. Earlier files
remain current until the child is ready and remain on disk afterward. See the
[library guide](library.md) for explicit baseline setup, cutoffs and Plex file
confirmation.

Selection and monitoring do not decode audio/video or prove release-name claims.
Search previews and submissions are also available in the [browser](web.md).
Durable [series monitoring and calendars](series.md) use these episode profiles.
Explicit [pack acquisitions](packs.md) use operator-provided file mappings.
Automatic pack search and general alternate/anime numbering remain the next
[stage](roadmap.md).
See [limits](limits.md) for remaining torrent,
integration and series constraints.
