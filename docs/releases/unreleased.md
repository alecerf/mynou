## Added: Connect Plex in one click

Settings, Plex & TMDB now has **Connect to Plex**: Plex's own page opens, you
sign in there, and Mynou keeps the token without showing it. It then lists the
servers your account owns, finds an address of the one you choose that answers
from this Mac (Plex on the same Mac first, then the local network), and picks
its film and series libraries, by matching their folders with Mynou's library
folders. Each library can be changed from a list of names; the server address,
the token and section numbers are no longer typed. See
[connect Plex](/docs/macos.md#connect-plex).

## Added: No password on the local network

Settings, Network can let computers on your local network use Mynou without
signing in. Only private and link-local addresses count; any other computer
still needs the password, and without one it cannot sign in. Network access no
longer needs a password when this is on. See
[network access](/docs/macos.md#network-access).

## Added: Statistics on the overview

The overview now shows how many films, series and episodes Plex holds; your
watchlist and how much of it is already in Plex; requests by kind, fetched by
Mynou or already in Plex; downloads, speeds and share ratio; the health of the
VPN, the watchlist and the indexers, with recent failures; each followed series
with its aired episodes and the next one to air; and a chart of what Mynou
fetched each week. `GET /api/stats` serves the same figures. See
[everyday use](/docs/macos.md#everyday-use).

## Changed: Choose what goes through the VPN

Settings, VPN now has a switch for each use of the VPN: **Torrents**,
**Indexers** (IRC sources included) and **TMDB**. Torrents go through the VPN
unless you turn it off; indexers and TMDB only if you turn them on. Without a VPN
profile, or with the VPN off, everything uses your own connection: a new
installation no longer waits for a VPN choice before it does anything. While the
VPN connects, only what goes through it waits; the Plex watchlist, TMDB and the
rest start at once. The per-source `"protection"` setting gives way to the
Indexers switch, and is no longer read. See
[protected sources](/docs/protected-sources.md).

## Changed: Only the current storage format opens

Mynou no longer converts the storage of earlier builds. Jobs, series,
requests, feeds, source policies, IRC history and transfers open only in their
current format; anything older, including the journal and snapshot files that
came before the disk indexes, is refused at startup instead of being migrated.
To start over, stop Mynou and move the data directory aside. Startup no longer
reports migration progress. See [inventory storage](/docs/inventory-storage.md).

## Fixed: Library posters match their title

A film or series in the library found its poster by searching TMDB for its
name and keeping the result with the same TMDB id; a title outside the first
results showed no poster. Mynou now asks TMDB for that id directly
(`GET /api/catalog?kind=movie&tmdb_id=N`).

## Fixed: Storage files stop growing after deletions

Pages that deleted records left empty in a store's index were never used
again, so the index file grew with every delete and re-add. They are now
reused before the file grows. The file still does not shrink.

## Changed: Folders are full paths

The movies, series and download folders are shown and saved as full paths on
the Mac running Mynou. The Settings view refuses a relative path; `~/` is your
home folder. An older relative setting reads as before, under the data
directory, and is saved as a full path the next time settings are saved.

## Fixed: A damaged extra no longer fails an import

When a download held several media files, a single one that could not be read,
such as a truncated sample, failed the whole import. Such files are now skipped;
the import fails only if no file with video remains, and its message names the
file that could not be read. Files are compared by size first, so a film is
analyzed alone instead of with every extra, and a season pack analyzes only the
file named for the episode.

## Fixed: Long MP4 films import again

An MP4 whose frames have uneven durations, as after a remux from Matroska or
from a phone at a variable frame rate, was refused once it held more than
100,000 timing entries, about two hours of film: its import failed with
"Invalid or oversized MP4 timing table". Such files are now read.

## Fixed: AVI duration and audio, QuickTime language

An AVI larger than 1 GiB reported the duration of its first gigabyte only; the
total now comes from its OpenDML header. Multichannel audio stored as
`WAVE_FORMAT_EXTENSIBLE` is named by its codec, such as `ac3`, instead of
`wav_0xfffe`. QuickTime files that give their language as a Macintosh code now
report it, such as `eng`.

## Changed: Media files are analyzed in one read or a few

Analyzing a file no longer walks through its media data. A two-hour MKV went
from about 3,000 scattered reads (47 MB, half a second on an internal SSD,
seconds on a hard disk or network share) to one 16 KiB read; an AVI also needs
one read, and an MP4 a few, with no long table read. See the
[media benchmark](/docs/media-benchmark.md).

## Changed: Updates must be signed

Mynou now installs an update only when the release key signed the release's
`SHA256SUMS`. The key never reaches GitHub, so a compromised account or build
cannot push an update to your Mac. Releases carry `SHA256SUMS.sig`; see
[verify release assets](/docs/validation.md#verify-release-assets).

## Added: Sign out everywhere

Settings, Network, on the Mac, can sign out every other computer and phone at
once, for example after losing one, without changing the password. See
[network access](/docs/macos.md#network-access).

## Changed: Failed sign-ins no longer lock everyone out

Only the address that failed waits after 5 wrong passwords; another computer can
no longer block your phone by failing on purpose. An IPv6 /64 counts as one
address.

## Changed: Secret names start with MYNOU_

Settings name secrets, and the environment variables Mynou falls back to, only
with the `MYNOU_` prefix, so settings cannot read the rest of the service's
environment.

## Changed: Each installation has its own Plex identity

Mynou now sends Plex an identifier of its own, made on first use, instead of
one shared by every installation, so Plex lists each installation as its own
device.

## Fixed: A requester retry survives a crash

Retrying a failed watchlist request no longer gets lost when Mynou stops
between recording the retry and queuing the request again.

## Changed: Lists page only by cursor

`GET /api/requesters/ID` and `GET /api/irc/announcements` no longer accept
`offset`; follow `next` as `after`. IRC history no longer repeats `next` as
`next_cursor`.

## Added: Choose the quality in Settings

Settings has a new **Quality** page. For films and for series, choose a quality
(4K when available, up to 1080p, up to 720p, or any), an audio language,
preferred or required, and the smallest and largest size you accept. Words to
avoid and to prefer in release names apply to both. A film or a series can use
another quality from its own sheet. See
[release selection profiles](/docs/selection.md).

## Added: Sizes and freeleech from indexers

Mynou now reads the size of each release, and whether it is freeleech, from
the indexers that say so. Profiles can set `min_size_mb`, `max_size_mb` and
`allow_unknown_size`; search previews show each release's size. See
[how a candidate is judged](/docs/selection.md#how-a-candidate-is-judged).

## Added: Rules for each indexer

Each indexer can have a priority (between equally good releases, the lower
number wins), its own minimum of seeders, a preference for freeleech releases,
and its own seeding goal (ratio and time) for what it downloads. Set them under
**Rules for this indexer** in Settings, Indexers. See
[sources](/docs/sources.md).

## Added: Turn downloads off

Settings, Downloads has a **Download torrents** switch. While it is off, nothing
downloads or seeds, and requests wait without searching or using up their
attempts; titles already in Plex are still found and marked ready. This helps
when you move an existing library to Mynou. The overview says when downloads
are off, and `GET /api/status` reports `downloads_enabled`.
