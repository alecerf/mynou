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
