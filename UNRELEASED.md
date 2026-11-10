## Added: Connect Plex in one click

Settings, Plex & TMDB now has **Connect to Plex**: Plex's own page opens, you
sign in there, and Mynou keeps the token without showing it. It then lists the
servers your account owns, finds an address of the one you choose that answers
from this Mac (Plex on the same Mac first, then the local network), and picks
its film and series libraries, by matching their folders with Mynou's library
folders. Each library can be changed from a list of names; the server address,
the token and section numbers are no longer typed.

## Added: No password on the local network

Settings, Network can let computers on your local network use Mynou without
signing in. Only private and link-local addresses count; any other computer
still needs the password, and without one it cannot sign in. Network access no
longer needs a password when this is on.

## Added: Statistics on the overview

The overview now shows how many films, series and episodes Plex holds; your
watchlist and how much of it is already in Plex; requests by kind, fetched by
Mynou or already in Plex; downloads, speeds and share ratio; the health of the
VPN, the watchlist and the indexers, with recent failures; each followed series
with its aired episodes and the next one to air; and a chart of what Mynou
fetched each week. `GET /api/stats` serves the same figures.

## Changed: Choose what goes through the VPN

Settings, VPN now has a switch for each use of the VPN: **Torrents**,
**Indexers** (IRC sources included) and **TMDB**. Torrents go through the VPN
unless you turn it off; indexers and TMDB only if you turn them on. Without a VPN
profile, or with the VPN off, everything uses your own connection: a new
installation no longer waits for a VPN choice before it does anything. While the
VPN connects, only what goes through it waits; the Plex watchlist, TMDB and the
rest start at once. The per-source `"protection"` setting gives way to the
Indexers switch, and is no longer read.

## Changed: Only the current storage format opens

Mynou no longer converts the storage of earlier builds. Jobs, series,
requests, feeds, source policies, IRC history and transfers open only in their
current format; anything older, including the journal and snapshot files that
came before the disk indexes, is refused at startup instead of being migrated.
To start over, stop Mynou and move the data directory aside. Startup no longer
reports migration progress.

## Fixed: IRC downloads respect the maximum size

An IRC announcement was admitted before the size of its file was known, and the
quality's maximum size was not checked again once it was. A file larger than
the maximum is now refused (`profile_size_rejected`) before anything is
downloaded.

## Fixed: Stalled peers and handshakes time out

A peer sending slow keepalives could hold a piece request forever; only new
piece data now extends it. Through the VPN, a client repeating its SYN no
longer keeps a half-open connection waiting past its timeout. A TLS 1.2 server
that never finishes its handshake is refused instead of answered.

## Fixed: IRC cleanup no longer deletes files in use

The hourly cleanup of IRC metadata could remove a file that a routing pass had
just refreshed or was still writing. Cleanup now waits for the pass to finish.

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
one read, and an MP4 a few, with no long table read.

## Changed: Updates must be signed

Mynou now installs an update only when the release key signed the release's
`SHA256SUMS`. The key never reaches GitHub, so a compromised account or build
cannot push an update to your Mac. Releases carry `SHA256SUMS.sig`.

## Added: Sign out everywhere

Settings, Network, on the Mac, can sign out every other computer and phone at
once, for example after losing one, without changing the password.

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

## Added: Quality profiles

Settings has a new **Quality** page with profiles, as in Radarr and Sonarr but
in plain words. A profile lists the qualities a release may have, best first,
the sources it accepts, and where upgrades stop (**Upgrade until**, a quality
and optionally a source). **Languages** lists what a release must have, and
the languages decide before the quality. Sizes are per hour of video, from the
length TMDB gives, and a release of unknown size can be skipped. HDR, Dolby
Vision and 3D are named preferences; words to prefer, avoid or require are
still there. Proper and repack releases are preferred and taken as upgrades.
**Try it** searches your indexers with a profile before you save it.

Films and series share the profiles; each kind has a default, and a title can
use another one, picked when you add or follow it, or later from its sheet. In
the release list, each release that came after Mynou's pick says why. A fresh
installation starts with Compact 1080p, Balanced 1080p and Cinema 4K.

## Added: French dubs by their tags

French releases are no longer all "French": Mynou reads **VFF** (France, also
TRUEFRENCH), **VFQ** (Québec) and **VFI** (international) apart, and **VF2** as
both VFF and VFQ; FRENCH or VF alone stays French without a dub. **VOSTFR**
is the original audio with French subtitles: VOSTFR, SUBFRENCH or French
subtitles in the name, and MULTi with a French audio (MULTi.VFF counts as VFF
and VOSTFR).

A profile's **Languages** are all needed: VFF and VOSTFR take a release with
both. French (any French) matches every dub. The languages decide before the
quality: a 1080p MULTi.VFF beats a 4K in VFF alone, and an upgrade never trades
a language for a better quality. **Missing some** says what happens to a
release with only some of them: accepted, the most complete one comes first
(the first languages counting most) and an upgrade replaces it once a release
has them all; refused, only a complete release is downloaded, and **MULTI and
untagged** says whether a release that does not name its languages may still
come. **MULTI first** puts releases that also hold the original audio first.
The release list shows each release's tags.

## Changed: An upgrade replaces the file

An upgrade takes the place of the file it replaces, under the usual name
(`Film (2024).mkv`, `Show - S01E02.mkv`), instead of being added beside it as
`… [mynou-…].mkv`. The new file replaces the earlier one in one step once it
is complete and checked. When the extension differs (an `.mp4` upgraded by an
`.mkv`), the earlier file is deleted once the upgrade is in the library (and
in Plex, when Plex is on). If that deletion fails, the earlier file stays in
Storage, where it can be deleted.

## Changed: Languages, then quality, then preferred words

A release's languages decide first, then its quality and source; preferred
words, proper and repack, then the codec break ties. Before, a preferred word
ranked above the resolution, and a profile's languages were alternatives
rather than all needed: a profile that listed several dubs as fallbacks (VFF,
then VFI, then French) now needs them all, so keep only French for any French
dub.

## Fixed: A preferred language no longer rejects the others

With **Only this language** off, a release marked in another language was
rejected although the setting promised it as a fallback. With **Missing some**
accepted, such a release now comes after those with your languages.

## Added: Sizes and freeleech from indexers

Mynou now reads the size of each release, and whether it is freeleech, from
the indexers that say so. Profiles can set `min_size_mb`, `max_size_mb` and
`allow_unknown_size`; search previews show each release's size.

## Added: Rules for each indexer

Each indexer can have a priority (between equally good releases, the lower
number wins), its own minimum of seeders, a preference for freeleech releases,
and its own seeding goal (ratio and time) for what it downloads. Set them under
**Rules for this indexer** in Settings, Indexers.

## Added: Turn downloads off

Settings, Downloads has a **Download torrents** switch. While it is off, nothing
downloads or seeds, and requests wait without searching or using up their
attempts; titles already in Plex are still found and marked ready. This helps
when you move an existing library to Mynou. The overview says when downloads
are off, and `GET /api/status` reports `downloads_enabled`.

## Changed: Signing out ends the session on the Mac

Signing out used to forget the cookie in the browser only; a copy of it kept
working for 30 days. Mynou now remembers signed-out sessions until they expire
and refuses them.

## Fixed: A proxy in front of Mynou no longer skips the password

A request forwarded by a reverse proxy (it carries `Forwarded`,
`X-Forwarded-For`, `X-Forwarded-Host`, `X-Real-IP` or `Via`) is no longer taken
as coming from this Mac or from the local network, so it needs a session, and
it cannot change the password.

## Changed: Imports on the same disk are instant

When the library is on the same APFS volume as the downloads, an import is now
a copy-on-write clone: a 4 GiB film is filed in about 30 ms instead of seconds,
and takes no extra space until one of the two files changes. On another disk,
the copy is checked by reading it back once, no longer by reading the download
a second time as well.

## Added: Free up space

Library, **Free up space** opens Storage: the earlier versions an upgrade
replaced and the finished downloads, with their size. Mynou still deletes
nothing by itself; each one goes only when you ask, and Plex is asked to scan
again.

## Added: Peer exchange in both directions

Mynou already learned peers from the others through PEX; it now also tells
them, once a minute, which peers it is connected to, as other clients do.
Private torrents still never use PEX.

## Added: A DHT node on the BitTorrent port

Mynou now answers other DHT nodes on its BitTorrent UDP port (through the
forwarded VPN port when the VPN is on) and keeps the peers announced to it, so
it takes part in the DHT instead of only asking it. Its own lookups say they
are read-only, so other nodes no longer keep their short-lived sockets.

## Added: uTP peers

Mynou now also talks to peers over uTP, the UDP transport most BitTorrent
clients offer, on the same port as TCP. It still dials TCP first; when a peer
is slow to answer or refuses TCP, uTP is tried as well. uTP transfers back off
when they start to fill the line, so a download disturbs other traffic less.
Peers that only accept uTP can now be reached.

## Changed: Faster peer turnover through the VPN

ProtonVPN's VPN Accelerator accepts every connection itself, so a peer that
is gone still seems to connect. Mynou now waits 3 seconds instead of 5 for a
peer's answer while downloading, so such peers are left sooner. Live peers
answered within 0.8 seconds in measurements.

## Changed: Season packs only for finished seasons

With **Prefer season packs** on, Mynou looks for a pack only once every episode
of a season has aired, as Sonarr does; a season that is still airing gets its
new episodes one by one.

## Fixed: Slow season packs no longer block a series

When season packs took too long to resolve, a series with two missing seasons
or more could get no episode at all, on every check. Pack searches now share
their time between the seasons and leave some for the episodes; the episodes
no pack covers are always requested, and a failed pack search shows on the
series.

## Changed: No limit for peers and downloads at once

**Peers per download** and **Downloads at once** can be left empty for no
limit. There is no longer a fixed cap of 256 connections: a download takes as
many peers as its swarm offers and its memory budget allows. The launchd
service raises its open-file limit to 10,240 so these connections fit;
reinstall the service to get it.

## Fixed: Downloads queue again with a high limit

With **Downloads at once** above 168, or with no limit, the download queue
could not be read and queued downloads never started after a restart.

## Changed: The year goes in the search text

The request page no longer has a Year field: type it after the title, as in
`Dune 2021` or `Dune (2021)`. A title that ends with a number, such as
`Wonder Woman 1984` or `1917`, is still found.

## Changed: The top bar speaks only when the connection is down

The status at the top right no longer says "All quiet. All connected."; it
appears while Mynou connects or reconnects.

## Changed: Imports go into the title's Plex folder

When Plex already lists a film or a series, Mynou files the new file in the
folder Plex keeps it in, such as `Mission - Impossible (1996)`, and an episode
in the folder that already holds its season, whether it is called `Season 2`,
`Season 02` or `Specials`. A new title still gets `Title (Year)` and
`Season 02` (`Season 00` for specials), but a colon in its name now becomes
` - `, as Plex names it, instead of `_`; a title Mynou already filed under the
old name keeps its folder. An upgrade goes where the file it replaces is. A
folder outside the library folders, one reached through a symbolic link, or a
title Plex lists in several folders gets Mynou's own layout instead. The job
says which folder it used and why: in its **Files & quality** tab, and as
`import_notes` in the API.

## Added: Subtitles come with the video

The subtitles of a release (`.srt`, `.ass`, `.ssa`, `.vtt`, and `.idx` with
its `.sub`) are imported next to the video under Plex's names, with the
language and the forced or SDH flags read from their names:
`Film (2024).fr.srt`, `Film (2024).en.forced.srt`. Other files, such as `.nfo`,
are left out. A subtitle already in the library is never replaced, by an import
or by an upgrade, and the job notes the one it kept. The job lists the
subtitles it imported (`subtitles` in the API).

## Added: Imports check the length of the file

Before filing a file, Mynou compares its length with the runtime TMDB gives for
the film or the episode, and accepts 70% to 160% of it: an extended cut passes,
a 22-minute file sold as a two-hour film does not. Outside that range nothing
is written; the job fails with `runtime_mismatch` and both lengths, and the
release is not picked again for this title by searches, feed watching or
upgrade checks (`rejected_releases` in the API, **Refused releases** in the
job's Acquisition tab). A file given with `source_path` is checked the same
way. When the runtime or the file's length is unknown, the file is imported
and the job notes that the check was skipped. A file holding several episodes
is not checked.

## Added: Your Plex friends' watchlists

Settings, Plex & TMDB lists your Plex friends, read with your own Plex
connection: friends have nothing to set up and share no token. Each friend has
a switch, off for new friends; once on, the films and series on their
watchlist become requests, with the friend as requester. Their state shows next
to the switch, with Plex's reason when a watchlist cannot be read, for example
a private one. A title on several watchlists, yours included, is one request
that stays while anyone still wants it. Once a friend is on, your own watchlist
is read as a requester too, as `plex-<your Plex account number>`, unless a
requester account already reads it. Turning a friend off, or a friend leaving
your friends on Plex, stops reading their list and keeps what was already
requested; a renamed friend stays the same requester. Friends are requesters
named `plex-<Plex account number>` in `GET /api/requesters`, which also lists
them with their state; `POST /api/plex/friends` reads the list again and
`POST /api/plex/friends/<id>` with `{"enabled": true}` turns one on. Requester
accounts of the settings can no longer have an ID starting with `plex-`.
