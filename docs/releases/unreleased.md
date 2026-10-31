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

## Changed: Choose what goes through the VPN

Settings, VPN now has a switch for each use of the VPN: **Torrents**,
**Indexers** (IRC sources included) and **TMDB**. Torrents go through the VPN
unless you turn it off; indexers and TMDB only if you turn them on. Without a VPN
profile, or with the VPN off, everything uses your own connection: a new
installation no longer waits for a VPN choice before it does anything. While the
VPN connects, only what goes through it waits; the Plex watchlist, TMDB and the
rest start at once. The per-source `"protection"` setting gives way to the
Indexers switch; an older document that protected a source turns it on. See
[protected sources](/docs/protected-sources.md).

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
