## Added: Invite-only IRC announce channels

An IRC source can now have an invite command, for private trackers whose
announce channel is invite-only: the bot's nickname and a message such as
`invite {secret}`, whose key is saved like the other secrets. Once registered
and identified (SASL or NickServ), Mynou sends the message to the bot, waits up
to 10 seconds for its invitation, then joins, and does the same after every
reconnect. When the channel still refuses the join, Mynou asks the bot again
three times, waiting longer each time, then reports the source as failed with
the reason and tries again after the source's longest reconnect delay. A source
without an invite command now says that the channel needs one. The key is never
shown, logged or recorded. The command is set on the source (`"invite"`)
through `POST /api/settings`; IRC sources have no settings page yet.

## Fixed: IRC problems show on the overview

The overview looked for IRC errors where the service does not put them, so a
failing IRC source never showed there. It now does, with its reason when Mynou
knows it.

## Added: Cross-seed single episodes and releases with extras

Cross-seed discovery now finds a single episode whose video is a file of a
season pack you completed, and also searches each pack's season for them. A
candidate that lacks a few small files, such as an `.nfo` or a sample, is now
admitted: those files are downloaded into its own folder, up to **Cross-seed
extras** in Settings, Downloads (50 MB by default), and deleted with it. Video
is never downloaded and every piece is still verified; a candidate whose reused
video disagrees fails instead. Torrents with v2 metadata are still not
cross-seeded. In `POST /api/transfers/{id}/cross-seed`, a `null` entry in
`mapping` marks a file to download.

## Changed: New transfers ask their tracker again sooner

While the tracker of a new transfer answers with an error, as a private tracker
does until it lists a new upload, Mynou now asks it again every 7 seconds for
10 minutes instead of every 30 seconds, so a fresh cross-seed starts seeding
sooner. Transfers restored at startup keep the usual wait.

## Added: Original language in quality profiles

A quality profile can now ask for **Original language**: each title's own
audio, as TMDB gives it. VO, MULTi and VOSTFR releases hold it; a release
without a language tag counts unless the title is French; FRENCH, VFF, VFQ and
TRUEFRENCH count only for a French title. One profile now fits French and
English films alike: a French dub alone of an English film is refused, and a
French film's FRENCH release is taken. It combines with the other languages,
all of them still required: with VFF and Original language, an English film
needs MULTi with VFF.

Mynou keeps the original language with each film, series and episode once
TMDB gave it, and looks it up for titles added earlier when their profile
asks for it. While TMDB is off or does not answer, only VO, MULTi and VOSTFR
count. **Choose a release** says for each release whether it holds the
original audio and why, and the Quality page's **Try it** can take a title's
original language to show the decision. Search reports carry
`original_language`, and each assessment its `original` decision.

## Added: Install with one command

Mynou now installs with one line in Terminal, which also upgrades an existing
installation:

```sh
curl -fsSL https://raw.githubusercontent.com/alecerf/mynou/trunk/install.sh | sh
```

It downloads the latest release, or the version you name, checks that the
release key signed its `SHA256SUMS` and that the executable matches it, and
only then installs `mynou` into `~/.local/bin` (or `MYNOU_INSTALL_DIR`), with
no sudo. It refuses any Mac without Apple Silicon, and when that folder is not
on your `PATH`, it gives you the line to add to `~/.zshrc`.

## Added: Several VPN profiles that take over from one another

Settings, VPN now keeps a list of up to 16 WireGuard profiles, such as several
servers of your provider: **Add a .conf** imports one more, named after its
file, and each can be moved up or down or removed. When the server in use stops
answering for a few minutes, Mynou moves to the next profile, wrapping around
the list, and asks the new server for a forwarded port, which peers and trackers
then learn. While it switches, torrents, and whatever else goes through the VPN,
wait; nothing falls back to your own connection. The page and the overview show
the profile in use, and **Recent switches** lists the last changes with their
time and reason. `GET /api/vpn` and `GET /api/stats` report the same. The
profile you had keeps working as a list of one.

## Changed: The VPN keeps trying after it gives up

Once every profile has failed in a row, or the only one has, Mynou no longer
waits for you to press **Try again**: it tries again after five minutes, then
waits twice as long each time, up to an hour, and starts over at five minutes
once the VPN works again. **Try again** still starts at once. Importing a
profile now adds it to the list instead of replacing the one you had.

## Added: Logs

Mynou now keeps a log in the data directory, under `logs/`: one line per
record, with its time, its level (error, warning, information or detail) and
where it comes from. Failed searches, indexer and feed errors, IRC
disconnections, VPN reconnections, downloads, imports and failed requests are
all there. The log never takes more than 5 MiB: five files of at most 1 MiB,
the oldest records making room for new ones. Tokens, passkeys, API keys,
passwords and cookies are replaced by `[redacted]` before anything is written.

Under the launchd service, what the process prints itself, such as a crash
report, no longer goes to `/dev/null`: it lands in the log too. A service
installed by an earlier version is switched over at its next start.

The overview's Services panel has **Read the logs**: the newest records first,
filtered by level, with **Download** to save them all. `GET /api/logs` and
`GET /api/logs/download` serve the same.
